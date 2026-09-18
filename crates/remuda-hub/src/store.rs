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
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};
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
    /// Current model id from create / `instance.configure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
        }
    }
}

/// Lifecycles that still occupy coordinator seats (design §2.5 uniqueness).
const ACTIVE_HOLDER_SQL: &str = "lifecycle NOT IN ('exited', 'failed', 'closed')";

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
                    "SELECT 1 FROM instances WHERE {ACTIVE_HOLDER_SQL}
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
                        "SELECT 1 FROM instances WHERE {ACTIVE_HOLDER_SQL}
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
        let active_children: i64 = conn.query_row(
            "SELECT COUNT(*) FROM instances
             WHERE spec_json LIKE ?1
               AND lifecycle NOT IN ('exited', 'failed', 'closed')",
            params![format!("%\"parentInstanceId\":\"{pid}\"%")],
            |row| row.get(0),
        )?;
        if u32::try_from(active_children).unwrap_or(0) >= limits.fan_out {
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
    /// Protocol §2.5 settlement projection, present only once settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<CommandSettlement>,
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
        json!({
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
    /// Open (or create) `hub.sqlite` on a dedicated writer thread, plus a
    /// read-only pool for reads that can be long (hub-store-1).
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
    pub async fn expire_lost_hosts(&self, grace_ms: u64) -> Result<usize, StoreError> {
        self.run_named("expire_lost_hosts", move |conn| {
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

    /// Permanently delete an Instance and everything the Hub keeps for it.
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
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM journal WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            tx.execute(
                "DELETE FROM commands WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            tx.execute(
                "DELETE FROM interactions WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            tx.execute(
                "DELETE FROM fleet_members WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            tx.execute("DELETE FROM instances WHERE id = ?1", params![&instance_id])?;
            // Tombstone: a Node command that is still draining will keep
            // appending journal events for this id, and `ensure_instance`
            // would happily recreate the row. A deleted session must stay
            // deleted, so the id is refused from here on.
            tx.execute(
                "INSERT OR REPLACE INTO deleted_instances (instance_id, deleted_at)
                 VALUES (?1, ?2)",
                params![&instance_id, now_rfc3339()],
            )?;
            tx.commit()?;
            Ok(true)
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
    ) -> Result<Vec<String>, StoreError> {
        self.run_named("reconcile_reported_instances", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM instances
                 WHERE host_id = ?1 AND lifecycle NOT IN ('exited', 'failed', 'requested')",
            )?;
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
                conn.execute(
                    "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                        last_error = ?1, updated_at = ?2 WHERE id = ?3",
                    params![&reason, &now, id],
                )?;
            }
            Ok(lost)
        })
        .await
    }

    /// Expire `requested` instances that never produced a Node receipt.
    ///
    /// A create the Node never acknowledged keeps occupying a placement slot
    /// forever otherwise. Returns `(host_id, instance_id)` for each expiry so
    /// the caller can publish a diagnostic.
    pub async fn expire_stale_requested(
        &self,
        window_ms: u64,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.run_named("expire_stale_requested", move |conn| {
            let now = now_rfc3339();
            let window = window_ms.min(i64::MAX as u64) as i64;
            let mut stmt = conn.prepare(
                "SELECT id, host_id FROM instances
                 WHERE lifecycle = 'requested' AND durable_seq = 0 AND
                    (julianday(?1) - julianday(created_at)) * 86400000 >= ?2",
            )?;
            let stale: Vec<(String, String)> = stmt
                .query_map(params![&now, window], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            for (id, _) in &stale {
                conn.execute(
                    "UPDATE instances SET lifecycle = 'failed', activity = 'idle',
                        last_error = 'create-never-acknowledged', updated_at = ?1
                     WHERE id = ?2 AND lifecycle = 'requested'",
                    params![&now, id],
                )?;
            }
            Ok(stale
                .into_iter()
                .map(|(id, host)| (host, id))
                .collect::<Vec<_>>())
        })
        .await
    }

    /// Settle a stop/close for an instance the Node does not know.
    ///
    /// Hub projection only: the row moves to `exited` so the slot is released
    /// and the caller never waits on a receipt that will not arrive.
    pub async fn settle_instance_exited(
        &self,
        instance_id: String,
        reason: String,
    ) -> Result<bool, StoreError> {
        self.run_named("settle_instance_exited", move |conn| {
            let changed = conn.execute(
                "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                    last_error = ?1, updated_at = ?2
                 WHERE id = ?3 AND lifecycle NOT IN ('exited', 'failed')",
                params![reason, now_rfc3339(), instance_id],
            )?;
            Ok(changed > 0)
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
        )
        .await
    }

    /// Insert with explicit delegation-tree state; design §2.5.
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
    ) -> Result<InstanceRecord, StoreError> {
        self.run_named("insert_instance_delegated", move |conn| {
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
            conn.execute(
                "INSERT INTO instances
                    (id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                     title, journal_id, durable_seq, spec_json, created_at, updated_at,
                     role, scope_json, grants_json, task_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'requested', 'unknown', ?6,
                         ?7, ?8, 0, ?9, ?10, ?10,
                         ?11, ?12, ?13, ?14)",
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
                ],
            )?;
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
    pub async fn fail_instance(
        &self,
        instance_id: String,
        last_error: String,
    ) -> Result<(), StoreError> {
        self.run_named("fail_instance", move |conn| {
            conn.execute(
                "UPDATE instances
                 SET lifecycle = 'failed', last_error = ?1, updated_at = ?2
                 WHERE id = ?3",
                params![last_error, now_rfc3339(), instance_id],
            )?;
            Ok(())
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
        self.run_named("queue_command", move |conn| {
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
        self.run_named("mark_forward_intent", move |conn| {
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
                        settlement_outcome, settlement_reason
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
        self.run_named("reject_command", move |conn| {
            let now = now_rfc3339();
            let changed = conn.execute(
                "UPDATE commands
                 SET state = 'settled', resolution = 'clear',
                     settlement_outcome = 'rejected', settlement_reason = ?1, updated_at = ?2
                 WHERE id = ?3 AND state = 'queued'",
                params![reason, now, command_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            load_command(conn, &command_id)
        })
        .await
    }

    /// Append a mirrored event. `seq` None assigns durableSeq+1.
    pub async fn append_journal(
        &self,
        host_id: String,
        instance_id: String,
        seq: Option<i64>,
        event: Value,
    ) -> Result<JournalAppend, StoreError> {
        self.run_named("append_journal", move |conn| {
            let inst = load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))?;
            if inst.host_id != host_id {
                return Err(StoreError::Id("instance belongs to another host".into()));
            }
            let durable = inst.durable_seq.parse::<i64>().unwrap_or(0);
            let mut cursor = AppendCursor::new(durable, seq);
            append_loaded_event(conn, &host_id, &instance_id, &mut cursor, event)
        })
        .await
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
            let tx = conn.transaction()?;
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

    /// Operator PATCH of labels / maxInstances / display name / provider binding (does not mark online).
    pub async fn patch_host(
        &self,
        host_id: String,
        name: Option<String>,
        labels: Option<Value>,
        max_instances: Option<i64>,
        provider_binding: Option<String>,
        launch_defaults: HostLaunchDefaultsPatch,
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
                            supply_json
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
                            supply_json
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
                  created_at, updated_at, scope, supply_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?12, ?12, ?13, ?14)",
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
                supply_json
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
            updated_at TEXT NOT NULL,
            settlement_outcome TEXT,
            settlement_reason TEXT
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
        apply_effective_model_projection(conn, instance_id, effective, payload.get("catalog"))?;
    }
    if kind == "permission"
        && let Some(effective) = payload.get("effective")
    {
        apply_effective_permission_projection(conn, instance_id, effective)?;
    }
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
        conn.execute(
            "UPDATE instances SET spec_json = json_set(spec_json, '$.nativeSignalTier', ?1) WHERE id = ?2",
            params![payload.pointer("/entity/nativeRef/signalTier").and_then(Value::as_str), instance_id],
        )?;
    }
    if kind == "lifecycle" && payload_type == "native" {
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

        let expired = store
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
                json!({"kind":"lifecycle","payload":{"type":"entity","state":"ready"}}),
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
                json!({"kind":"lifecycle","payload":{"type":"entity","state":"exited"}}),
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
            .patch_host(host.clone(), None, None, Some(2), None, Default::default())
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
        let reconciled = store
            .reconcile_reported_instances(
                host.clone(),
                vec![kept.instance_id.clone()],
                "node-epoch-changed".into(),
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
        );
        assert!(
            !store
                .settle_instance_exited(instance.instance_id.clone(), "node-lost-instance".into())
                .await
                .expect("settle again"),
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
                .patch_host(host.clone(), None, None, Some(32), None, Default::default())
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
                    COALESCE(max_instances_override, max_instances), hostname, provider_binding,
                    default_launch_args, claude_binary_path, default_tui
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
                    row.get::<_, Option<String>>(13)?
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "auto".into()),
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
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
        provider_binding,
        default_launch_args,
        claude_binary_path,
        default_tui,
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
    }))
}

fn load_instance(conn: &Connection, id: &str) -> Result<Option<InstanceRecord>, StoreError> {
    conn.query_row(
        "SELECT id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                title, journal_id, durable_seq, created_at, updated_at, spec_json, last_error,
                mode, promoted_at, launched_by,
                role, scope_json, grants_json, task_id
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
            let model = spec
                .get("model")
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
                model,
                tui: spec
                    .get("tui")
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
                effort_name,
                effort_ultracode,
                effort_index,
                effort_effective,
                model_effective,
                model_catalog,
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
    apply_instance_projection(conn, instance_id, &event, seq, &now)?;
    apply_native_session_projection(conn, instance_id, &event, &now)?;
    apply_command_projection(conn, host_id, instance_id, &event, &now)?;
    apply_interaction_event(conn, host_id, instance_id, &event)?;
    apply_instance_lifecycle(conn, instance_id, &event)?;
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

fn load_command(conn: &Connection, id: &str) -> Result<Option<CommandRecord>, StoreError> {
    conn.query_row(
        "SELECT id, instance_id, host_id, operation, state, resolution, forwarded,
                payload_json, idempotency_key, created_at, updated_at,
                settlement_outcome, settlement_reason
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
                settlement_outcome, settlement_reason
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
    if native_name == "SubagentStop" {
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
    let herdr_idle_proof = native_name == "agent_status"
        || native_name == "session"
        || payload_type == "native"
        || payload.get("entityType").and_then(Value::as_str) == Some("instance");
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
    let settlement_outcome: Option<String> = row.get(11)?;
    let settlement_reason: Option<String> = row.get(12)?;
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
