//! D-023: operator workspace registration, acknowledged Node snapshots, and audit journal.

use crate::auth::require_origin;
use crate::store::{Store, StoreError};
use crate::{AppState, HubError};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

/// Per-`(host, workspace)` unbind lock. Held across the occupancy check and
/// the whole prepare→settle unregister so two concurrent removals of one
/// directory cannot both pass the check before either commits (round 2
/// item 6). The Node additionally blocks new sessions on a prepared
/// workspace, closing the check→unbind window end to end.
fn unbind_locks() -> &'static std::sync::Mutex<HashMap<String, UnbindLock>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, UnbindLock>>> = OnceLock::new();
    LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// One per-workspace serialization mutex plus a settled flag.
struct UnbindLock {
    mutex: Arc<AsyncMutex<()>>,
    /// Set when the holding operation fully settles (or is rejected); the
    /// entry becomes prunable only then AND when the map is the sole holder
    /// of the Arc (no waiter cloned it).
    settled: std::sync::atomic::AtomicBool,
}

/// RAII guard for one unbind/admission operation. Acquiring clones the mutex
/// Arc (so the entry can never be pruned while a caller is queued or
/// holding), and Drop marks the entry settled on EVERY exit path — success,
/// rejection, or early return (c-dirpicker round 4 item 4).
pub(crate) struct OperationGuard {
    key: String,
    // The held guard is declared before the Arc so it is dropped first,
    // releasing the mutex before the Arc clone goes.
    _held: tokio::sync::OwnedMutexGuard<()>,
    _arc: Arc<AsyncMutex<()>>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Some(lock) = unbind_locks().lock().unwrap().get(&self.key) {
            lock.settled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Acquire the per-workspace serialization lock for one unregister or
/// binding admission, pruning only entries that are settled AND held by the
/// map alone (`Arc::strong_count == 1`). A waiter that cloned the Arc but
/// has not acquired the mutex yet still bumps strong_count, so its entry can
/// never be pruned out from under it and replaced by a new mutex (which would
/// split serialization).
pub(crate) async fn hold_workspace_operation(host_id: &str, workspace_id: &str) -> OperationGuard {
    let key = format!("{host_id}\u{1f}{workspace_id}");
    let arc = {
        let mut map = unbind_locks().lock().unwrap();
        // Opportunistic prune of settled entries the map uniquely owns.
        map.retain(|_key, lock| {
            !(lock.settled.load(std::sync::atomic::Ordering::Acquire)
                && Arc::strong_count(&lock.mutex) == 1)
        });
        let lock = map.entry(key.clone()).or_insert_with(|| UnbindLock {
            mutex: Arc::new(AsyncMutex::new(())),
            settled: std::sync::atomic::AtomicBool::new(false),
        });
        lock.settled
            .store(false, std::sync::atomic::Ordering::Release);
        lock.mutex.clone()
        // map lock released here; strong_count is at least 2 while `arc`
        // lives, so no other acquire can prune this entry.
    };
    let held = arc.clone().lock_owned().await;
    OperationGuard {
        key,
        _held: held,
        _arc: arc,
    }
}

/// One armed test barrier: the production handler signals `reached` when it
/// parks, then waits for `release` before continuing.
pub(crate) struct BarrierSlot {
    pub(crate) reached: Arc<tokio::sync::Notify>,
    pub(crate) release: tokio::sync::oneshot::Receiver<()>,
}

/// Test-only race barriers driving the REAL DELETE and POST /v1/tasks
/// handlers (round 6 item 3). The handler-side cost when nothing is armed is
/// one uncontended mutex lock and an empty-map lookup; production never arms.
#[derive(Default, Clone)]
pub(crate) struct RaceBarriers {
    /// Parks the unregister handler right after the occupancy query passes
    /// and before the command is queued / prepare is sent.
    unregister: Arc<std::sync::Mutex<std::collections::HashMap<(String, String), BarrierSlot>>>,
    /// Parks task creation right after it acquires the per-workspace guard
    /// and before the binding is published.
    task_bind: Arc<std::sync::Mutex<std::collections::HashMap<(String, String), BarrierSlot>>>,
}

impl RaceBarriers {
    /// Install `slot` for one (host, workspace) key.
    pub(crate) fn insert(&self, phase: RacePhase, key: (String, String), slot: BarrierSlot) {
        let table = match phase {
            RacePhase::Unregister => &self.unregister,
            RacePhase::TaskBind => &self.task_bind,
        };
        table.lock().unwrap().insert(key, slot);
    }

    /// If a barrier is armed for this key, signal that the handler reached
    /// the park point and wait for release. One-shot (removed on entry).
    pub(crate) async fn wait_if_armed(&self, phase: RacePhase, host_id: &str, workspace_id: &str) {
        let slot = {
            let table = match phase {
                RacePhase::Unregister => &self.unregister,
                RacePhase::TaskBind => &self.task_bind,
            };
            table
                .lock()
                .unwrap()
                .remove(&(host_id.to_owned(), workspace_id.to_owned()))
        };
        if let Some(slot) = slot {
            slot.reached.notify_waiters();
            // Park until the test releases; a dropped sender (test failed
            // away) closes the channel and the handler proceeds.
            let _ = slot.release.await;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RacePhase {
    Unregister,
    TaskBind,
}

/// Stable substrings the Node's unregister prepare emits for occupancy/race
/// refusals. These must reach the operator as 409 rather than the channel's
/// generic 400; keep them in sync with the Node messages in
/// `remuda-node/src/workspace.rs`.
const NODE_UNREGISTER_CONFLICT_MARKERS: &[&str] = &[
    "live session(s)",
    "being unregistered",
    "was replaced after unregister prepare",
    // Node unregister commit re-counts occupancy (round 4 item 10).
    "gained occupancy after unregister prepare",
    "no longer registered",
    // Round 6 item 1: the resolved id/root must match at both phases.
    "identity does not match the resolved workspace",
    "identity changed between prepare and commit",
    // r8 item 2: a prepare/commit arrived after the command's abort fence.
    "was aborted before",
];

fn node_unregister_conflict(reason: &str) -> Option<&str> {
    NODE_UNREGISTER_CONFLICT_MARKERS
        .iter()
        .find(|marker| reason.contains(**marker))
        .map(|_| reason)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/hosts/{id}/workspaces",
            get(list).post(register).delete(unregister),
        )
        // G2: read-only, operator-only proxies to the Node's real-time git
        // working-tree computation (files-view-contract §4). Nothing is
        // cached: offline hosts get 409/422 and no stale content is served.
        .route(
            "/v1/hosts/{id}/workspaces/{workspaceId}/changes",
            get(changes_status),
        )
        .route(
            "/v1/hosts/{id}/workspaces/{workspaceId}/changes/diff",
            get(changes_diff),
        )
        .route(
            "/v1/hosts/{id}/workspaces/{workspaceId}/changes/file",
            get(changes_file),
        )
}

#[derive(Deserialize)]
struct ChangesPath {
    id: String,
    #[serde(rename = "workspaceId")]
    workspace_id: String,
}

#[derive(Deserialize)]
struct DiffQuery {
    path: String,
    #[serde(default)]
    staged: bool,
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

/// `GET /v1/hosts/{hostId}/workspaces/{workspaceId}/changes` — current status.
async fn changes_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<ChangesPath>,
) -> Result<Json<Value>, HubError> {
    proxy_scm(&state, &headers, &path, "workspace.scm.status", json!({})).await
}

/// `GET …/changes/diff?path=…&staged=…` — one entry's unified diff.
async fn changes_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<ChangesPath>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<Value>, HubError> {
    proxy_scm(
        &state,
        &headers,
        &path,
        "workspace.scm.diff",
        json!({"paths": [query.path], "staged": query.staged}),
    )
    .await
}

/// `GET …/changes/file?path=…` — restricted current bytes for an entry.
async fn changes_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<ChangesPath>,
    Query(query): Query<FileQuery>,
) -> Result<Json<Value>, HubError> {
    proxy_scm(
        &state,
        &headers,
        &path,
        "workspace.scm.file",
        json!({"path": query.path}),
    )
    .await
}

/// Common proxy: operator gate, host existence, online gate (409), then the
/// Node call (`call_node` maps `Ok(None)` to 422 PLACEMENT_UNSATISFIABLE).
async fn proxy_scm(
    state: &AppState,
    headers: &HeaderMap,
    path: &ChangesPath,
    method: &str,
    mut node_params: Value,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(state, headers).await?;
    require_host(state, &path.id).await?;
    if state.nodes.kind_of(&path.id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: path.id.clone(),
        });
    }
    node_params["workspaceId"] = json!(path.workspace_id);
    let body = crate::http::call_node(state, &path.id, method, node_params).await?;
    Ok(Json(body))
}

#[derive(Deserialize)]
struct WorkspacePath {
    path: String,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    require_host(&state, &id).await?;
    if state.nodes.kind_of(&id).await.is_some() {
        let snapshot = crate::http::call_node(&state, &id, "workspace.list", json!({})).await?;
        observe_snapshot(&state, &id, snapshot, None).await?;
    }
    Ok(Json(snapshot_view(&state.store, id).await?))
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<WorkspacePath>,
) -> Result<Json<Value>, HubError> {
    mutate(&state, &headers, id, body, "workspace.register").await
}

async fn unregister(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<WorkspacePath>,
) -> Result<Json<Value>, HubError> {
    mutate(&state, &headers, id, body, "workspace.unregister").await
}

async fn require_host(state: &AppState, id: &str) -> Result<(), HubError> {
    state
        .store
        .get_host(id.to_owned())
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(())
}

/// A session stops occupying its workspace once its process has ended.
/// Both terminal lifecycles end occupancy: `exited` (clean close or
/// successful native exit) and `failed` (process ended with an error —
/// non-zero exit, signal or EOF; D-057 OA6: failed is itself process-end
/// evidence and never a turn-level error). Rows in every other lifecycle
/// (including `closed`, which no current Node emits) keep blocking.
/// Liveness for a legacy row written before c-cardsettle stays the Node's
/// job: its own live-session count refuses unbind while the session's
/// process is still running even if the Hub row says failed.
const ACTIVE_WORKSPACE_SESSION_SQL: &str = "lifecycle NOT IN ('exited', 'failed')";

/// Task states that no longer occupy the bound directory. A parked or
/// deferred task still holds its lease, so it keeps blocking removal; an
/// archived task of any state is considered settled.
const INACTIVE_TASK_STATES: &str = "('done', 'failed')";

/// Count live sessions and active (non-archived) tasks bound to one host's
/// workspace. c-dirpicker refuses an unregister while either count is non-zero.
async fn workspace_users(
    state: &AppState,
    host_id: &str,
    workspace_id: &str,
) -> Result<(i64, i64), HubError> {
    let host = host_id.to_owned();
    let workspace = workspace_id.to_owned();
    state
        .store
        .run_named("workspace_users", move |conn| {
            count_workspace_users(conn, &host, &workspace)
        })
        .await
        .map_err(HubError::from)
}

/// Plain SQL form of [`workspace_users`] (one writer transaction), also
/// covered by the in-memory unit tests below.
pub(crate) fn count_workspace_users(
    conn: &Connection,
    host_id: &str,
    workspace_id: &str,
) -> Result<(i64, i64), StoreError> {
    let sessions: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM instances
             WHERE host_id = ?1 AND workspace_id = ?2 AND {ACTIVE_WORKSPACE_SESSION_SQL}"
        ),
        params![host_id, workspace_id],
        |row| row.get(0),
    )?;
    let tasks: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM tasks
             WHERE archived_at IS NULL
               AND state NOT IN {INACTIVE_TASK_STATES}
               AND json_extract(doc_json, '$.workspaceBinding.hostId') = ?1
               AND json_extract(doc_json, '$.workspaceBinding.workspaceId') = ?2"
        ),
        params![host_id, workspace_id],
        |row| row.get(0),
    )?;
    Ok((sessions, tasks))
}

/// Resolve an unregister DELETE `path` to the Node-authoritative
/// `(workspaceId, canonicalRoot)` BEFORE taking the operation lock or
/// counting occupancy (round 5 item 2; round 6 item 1; r7 item 2).
///
/// The snapshot shortcut is strict STRING equality against the stored root
/// bytes: a real trailing space is a different name, nothing is trimmed, and
/// component-equivalent spellings (`/root/`, `/root/.`, `//root`) do NOT
/// match and go through the Node like any other alias. That RPC is the
/// READ-ONLY `workspace.resolve`, which runs `realpath` semantics on the
/// real filesystem and returns the matched registry row's STORED root.
/// A lexical `..` collapse is NEVER identity: `/allowed/link/../A` resolves
/// where the symlink actually points, so the browse RPC (`host.dirs.list`,
/// whose walk collapses `..` lexically) is never used to identify a removal
/// target. A Node refusal (symlink escape, missing, out-of-policy) returns
/// the error; unregister is not called and no snapshot is observed.
async fn workspace_identity_for_path(
    state: &AppState,
    host_id: &str,
    path: &str,
) -> Result<Option<(String, String)>, HubError> {
    let host = host_id.to_owned();
    let (_, workspaces) = state
        .store
        .run_named("workspace_identity_for_path", move |conn| {
            load_snapshot(conn, &host)
        })
        .await?;

    // (1) Exact stored root only: no trim, no lexical `..`.
    if let Some(workspace) = workspaces
        .iter()
        .find(|workspace| workspace["root"].as_str() == Some(path))
    {
        let id = workspace["workspaceId"]
            .as_str()
            .ok_or_else(|| HubError::BadRequest("snapshot workspaceId missing".into()))?;
        return Ok(Some((id.to_owned(), path.to_owned())));
    }

    // (2) Node-authoritative resolution of the exact path via the read-only
    //     unregister-twin RPC. Any error (offline host aside, which call_node
    //     raises on its own) refuses the DELETE before the lock and before any
    //     `workspace.unregister` frame is sent.
    let answer =
        crate::http::call_node(state, host_id, "workspace.resolve", json!({"path": path})).await?;
    let workspace_id = answer
        .get("workspaceId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HubError::BadRequest(
                "the Node could not resolve the directory to a registered workspace".into(),
            )
        })?;
    let canonical_root = answer
        .get("canonicalRoot")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HubError::BadRequest("the Node resolved no canonical workspace root".into())
        })?;
    // Cross-check the Node's answer against the observed snapshot: identity
    // and stored root must both be present there, otherwise the Hub projection
    // and the Node disagree and the unregister refuses.
    let known = workspaces.iter().any(|workspace| {
        workspace["workspaceId"].as_str() == Some(workspace_id)
            && workspace["root"].as_str() == Some(canonical_root)
    });
    if !known {
        return Err(HubError::BadRequest(
            "the Node resolved a workspace the Hub has not observed; refresh workspaces before \
             retrying"
                .into(),
        ));
    }
    Ok(Some((workspace_id.to_owned(), canonical_root.to_owned())))
}

/// Lexical normalization helper used only by unit tests now that unregister
/// identity resolution is exact-match + Node-authoritative (round 5 item 2).
#[cfg(test)]
fn normalize_absolute(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            name => stack.push(name),
        }
    }
    if stack.is_empty() {
        Some("/".to_owned())
    } else {
        let mut out = String::from('/');
        out.push_str(&stack.join("/"));
        Some(out)
    }
}

async fn mutate(
    state: &AppState,
    headers: &HeaderMap,
    id: String,
    body: WorkspacePath,
    method: &str,
) -> Result<Json<Value>, HubError> {
    require_origin(headers, &state.config)?;
    let device = crate::agent_scope::require_operator(state, headers).await?;
    require_host(state, &id).await?;
    if state.nodes.kind_of(&id).await.is_none() {
        return Err(HubError::HostOffline { host_id: id });
    }
    // c-dirpicker: never unbind a directory that a live session or an active
    // task still uses. For a known workspace the occupancy check, command
    // queue and prepare→settle run under one per-workspace lock, so a
    // concurrent removal of the same directory cannot pass the check too;
    // the Node's prepared-workspace create block closes the rest of the
    // window. Unknown paths skip the lock and fall through to the Node's
    // prepare rejection.
    // Round 5 item 1: resolve identity and take the per-workspace guard BEFORE
    // the occupancy query, then HOLD it in function scope through command
    // queue, prepare, commit and snapshot observation. A task create that
    // acquires the same guard (hold_workspace_operation around bind+publish)
    // therefore cannot interleave with this unregister anywhere.
    let unbind_identity = if method == "workspace.unregister" {
        workspace_identity_for_path(state, &id, &body.path).await?
    } else {
        None
    };
    let _operation_guard = if let Some((workspace_id, _)) = &unbind_identity {
        Some(hold_workspace_operation(&id, workspace_id).await)
    } else {
        None
    };
    if let Some((workspace_id, _canonical_root)) = &unbind_identity {
        let (sessions, tasks) = workspace_users(state, id.as_str(), workspace_id).await?;
        if sessions > 0 || tasks > 0 {
            let mut reasons = Vec::new();
            if sessions > 0 {
                reasons.push(format!("{sessions} live session(s)"));
            }
            if tasks > 0 {
                reasons.push(format!("{tasks} active task(s)"));
            }
            return Err(HubError::Conflict(format!(
                "directory {} is still used by {}; end or archive them before removing it \
                 (ended sessions keep their history)",
                body.path,
                reasons.join(" and ")
            )));
        }
        // Round 6 item 3: test-only park point between the occupancy query
        // and the prepare call, while the per-workspace guard is held. A
        // no-op in production (nothing armed).
        state
            .race_barriers
            .wait_if_armed(RacePhase::Unregister, &id, workspace_id)
            .await;
    }
    // Round 6 item 1: an unregister prepared from a resolve result sends the
    // EXACT stored canonical root bytes (never the operator's alias string)
    // plus the resolved workspaceId; the Node verifies both at prepare and at
    // commit before removing anything.
    let (node_path, node_workspace_id) = match &unbind_identity {
        Some((workspace_id, canonical_root)) if method == "workspace.unregister" => {
            (canonical_root.clone(), Some(workspace_id.clone()))
        }
        _ => (body.path.clone(), None),
    };
    let mut payload = json!({"path": node_path});
    // r8 item 1: persist the resolved workspaceId in the command payload so a
    // later reconnect abort sweep sends it on the abort frame. The frame only
    // carried path before, so the sweep never reconstructed the id the Node
    // verifies against (and tests had to inject one).
    if let Some(workspace_id) = &node_workspace_id {
        payload["workspaceId"] = json!(workspace_id);
    }
    crate::agent_scope::stamp(&mut payload, &device);
    let (command, _) = state
        .store
        .queue_command(None, None, id.clone(), method.into(), payload, None)
        .await?;
    state
        .store
        .mark_forward_intent(command.command_id.clone())
        .await?;
    let mut rpc_params = json!({
        "path": node_path,
        "commandId": command.command_id,
        "phase": "prepare",
    });
    if let Some(workspace_id) = &node_workspace_id {
        rpc_params["workspaceId"] = json!(workspace_id);
    }
    let prepared = crate::http::call_node(state, &id, method, rpc_params).await;
    let prepared = match prepared {
        Err(HubError::BadRequest(reason)) if method == "workspace.unregister" => {
            // The Node enforces the same occupancy rule at prepare (its view
            // is authoritative, e.g. a command queued while the host was
            // unreachable). Surface its busy/unbinding refusal as 409 with
            // the visible reason rather than the generic 400 channel error.
            let conflict = node_unregister_conflict(&reason).map(str::to_owned);
            if let Some(conflict) = conflict {
                mark_rejected(&state.store, &command.command_id, &id, &conflict).await?;
                return Err(HubError::Conflict(conflict));
            }
            mark_rejected(&state.store, &command.command_id, &id, &reason).await?;
            return Err(HubError::BadRequest(reason));
        }
        Err(HubError::BadRequest(reason)) => {
            mark_rejected(&state.store, &command.command_id, &id, &reason).await?;
            return Err(HubError::BadRequest(reason));
        }
        Err(error) if method == "workspace.unregister" => {
            // r7 item 1: a transport failure does not prove the prepare frame
            // never landed and set the mark. Abort (idempotent) before
            // returning; an unreachable host is reconciled on reconnect.
            abort_unregister(
                state,
                &id,
                &node_path,
                &node_workspace_id,
                &command.command_id,
            )
            .await;
            return Err(error);
        }
        result => result?,
    };
    require_phase(&prepared, &command.command_id, "prepared")?;
    state
        .store
        .mark_accepted(command.command_id.clone())
        .await?;
    state
        .store
        .mark_settlement_timed_out(command.command_id.clone())
        .await?;
    // Round 4 item 10: the Node re-counts occupancy at commit and can refuse
    // if a session/reservation raced in after prepare; map that conflict to
    // 409 like the prepare markers, instead of the generic channel 400.
    let mut commit_params = json!({
        "path": node_path,
        "commandId": command.command_id,
        "phase": "commit",
    });
    if let Some(workspace_id) = &node_workspace_id {
        commit_params["workspaceId"] = json!(workspace_id);
    }
    let settled = match crate::http::call_node(state, &id, method, commit_params).await {
        Ok(answer) => answer,
        Err(HubError::BadRequest(reason)) if node_unregister_conflict(&reason).is_some() => {
            // r7 item 1: a commit refusal must release the prepared unbinding
            // mark on the Node (its own commit refusal already cleared it;
            // this abort is the explicit, idempotent reconciliation).
            abort_unregister(
                state,
                &id,
                &node_path,
                &node_workspace_id,
                &command.command_id,
            )
            .await;
            return Err(HubError::Conflict(reason));
        }
        Err(error) => {
            // Link drop / timeout / restart: prepare may have landed and set
            // the mark. Best-effort abort (idempotent); if the host is gone
            // the reconnect sweep reconciles the command.
            abort_unregister(
                state,
                &id,
                &node_path,
                &node_workspace_id,
                &command.command_id,
            )
            .await;
            return Err(error);
        }
    };
    require_phase(&settled, &command.command_id, "settled")?;
    // Round 5 item 2: verify the Node settled the SAME workspace identity we
    // resolved and locked, so a `..`/alias cannot unregister a different one.
    if method == "workspace.unregister"
        && let Some((expected_id, expected_root)) = &unbind_identity
    {
        let settled_id = settled.get("workspaceId").and_then(Value::as_str);
        let settled_root = settled
            .get("workspaces")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|row| {
                    row.get("workspaceId").and_then(Value::as_str) == Some(expected_id.as_str())
                })
            })
            .and_then(|row| row.get("root").and_then(Value::as_str));
        if settled_id != Some(expected_id.as_str())
            || (settled_root.is_some() && settled_root != Some(expected_root.as_str()))
        {
            abort_unregister(
                state,
                &id,
                &node_path,
                &node_workspace_id,
                &command.command_id,
            )
            .await;
            return Err(HubError::Conflict(format!(
                "unregister settled a different workspace than resolved ({expected_id}@{expected_root})"
            )));
        }
    }
    observe_snapshot(state, &id, settled.clone(), Some(command.command_id)).await?;
    let mut response = snapshot_view(&state.store, id.clone()).await?;
    if let Some(workspace_id) = settled.get("workspaceId").cloned() {
        response["workspaceId"] = workspace_id;
    }
    // `_operation_guard` is held to here (after snapshot observation) and
    // drops across every exit path, blocking task creation for the full
    // prepare→settle window.
    Ok(Json(response))
}

/// Send the idempotent `abort` phase for one prepared unregister command.
/// Best-effort: a refusal here is logged but never masks the original handler
/// error, and the reconnect sweep is the backstop.
///
/// r8 item 2: the Node is the authority on whether the unbind actually ran.
/// Two distinct acks:
///  - `phase: "aborted"` — positive evidence the membership was NOT removed;
///    settle the command `rejected` (r7 item 1).
///  - `phase: "settled"`  — commit had already completed on the Node while the
///    Hub timed out (race B). The workspace really is gone, so OBSERVE the
///    returned snapshot and settle `completed`. Recording "rejected" here would
///    leave a stale projection that a later `create_task` could bind against.
///  - any transport error — leave the row unsettled; the next hello retries.
async fn abort_unregister(
    state: &AppState,
    host_id: &str,
    node_path: &str,
    node_workspace_id: &Option<String>,
    command_id: &str,
) {
    let mut params = json!({
        "path": node_path,
        "commandId": command_id,
        "phase": "abort",
    });
    if let Some(workspace_id) = node_workspace_id {
        params["workspaceId"] = json!(workspace_id);
    }
    match crate::http::call_node(state, host_id, "workspace.unregister", params).await {
        Ok(answer) if answer.get("phase").and_then(Value::as_str) == Some("settled") => {
            // The commit really landed on the Node before the abort arrived.
            // Mark completed FIRST (the row is still accepted here), then
            // observe the post-commit snapshot so the Hub drops the
            // membership. observe_snapshot's command update leaves the
            // `completed` outcome intact; calling it first would settle the
            // row and make mark_settled a no-op (losing the outcome).
            if let Err(error) = state
                .store
                .mark_settled(command_id.to_owned(), host_id.to_owned())
                .await
            {
                tracing::warn!(%command_id, %error, "could not settle unregister completed");
            }
            if let Err(error) =
                observe_snapshot(state, host_id, answer, Some(command_id.to_owned())).await
            {
                tracing::warn!(%command_id, %error, "could not observe settled unregister snapshot");
            }
        }
        Ok(_) => {
            // The Node acked the abort with positive evidence the unbind did
            // not happen; settle queued OR accepted as rejected.
            if let Err(error) = state
                .store
                .settle_workspace_command_aborted(
                    command_id.to_owned(),
                    "workspace unregister aborted before commit".to_owned(),
                )
                .await
            {
                tracing::warn!(%command_id, %error, "could not record unregister abort");
            }
        }
        Err(error) => {
            tracing::warn!(
                %command_id,
                %error,
                "unregister abort not delivered; reconnect reconciliation will clear the mark"
            );
        }
    }
}

/// c-dirpicker r8 item 1: spawn the post-`node.hello` abort sweep as its OWN
/// task, after the new transport is registered and after the hello reply is
/// queued (mirrors [`crate::ws::spawn_egress_reinstall`]).
///
/// Running the sweep inline in the hello handler can never work: it is called
/// before `state.nodes.insert`, so `call_node` finds no link; a stale
/// half-open slot would instead block the hello for the RPC timeout; and the
/// hello handler runs inside the session read loop, so an inline RPC that
/// waits for its own reply deadlocks. The spawned task queues its abort frames
/// behind the hello reply on the bounded FIFO, and commands whose abort is not
/// delivered stay `queued`/`accepted` and are retried by the sweep on the
/// NEXT hello.
///
/// r9 item 2: `connected_at` is the reconnect instant (captured by the
/// carrier just after the hello). A DELETE accepted on the NEW link has a
/// command row created at/after that instant and must not be aborted — its
/// prepare/commit are in flight on the very link the sweep is using.
pub fn spawn_unregister_abort_sweep(state: &AppState, host_id: String, connected_at: String) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(error) =
            abort_unsettled_unregisters_on_reconnect(&state, &host_id, &connected_at).await
        {
            tracing::warn!(%host_id, %error, "post-hello unregister abort sweep failed");
        }
    });
}

/// Reconcile unregister commands left unsettled across a Node (re)connect
/// (r7 item 1): for every `workspace.unregister` command on this host still
/// `queued` or `accepted` that was queued BEFORE this link's hello
/// (`connected_at`), send the idempotent abort so the Node drops a durable
/// unbinding mark a dead previous connection prepared. Commands created at
/// or after `connected_at` belong to in-flight DELETEs on the new link and
/// are left strictly alone (r9 item 2). Called from the authenticated
/// `node.hello` path of both carriers.
pub async fn abort_unsettled_unregisters_on_reconnect(
    state: &AppState,
    host_id: &str,
    connected_at: &str,
) -> Result<(), HubError> {
    let pending = state
        .store
        .list_unsettled_workspace_unregisters(host_id, connected_at)
        .await?;
    for command in pending {
        let node_path = command
            .payload
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default();
        let workspace_id = command
            .payload
            .get("workspaceId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        abort_unregister(
            state,
            host_id,
            &node_path,
            &workspace_id,
            &command.command_id,
        )
        .await;
    }
    Ok(())
}

async fn mark_rejected(
    store: &Store,
    command_id: &str,
    host_id: &str,
    reason: &str,
) -> Result<(), StoreError> {
    let command = command_id.to_owned();
    let host = host_id.to_owned();
    let reason = reason.to_owned();
    store.run_named("mark_rejected", move |conn| {
        let tx = conn.transaction()?;
        let now = crate::config::now_rfc3339();
        tx.execute("UPDATE commands SET resolution = 'clear', updated_at = ?1 WHERE id = ?2 AND state = 'queued'", params![now, command])?;
        let event = json!({"type":"workspace.rejected", "hostId":host, "commandId":command, "reason":reason});
        tx.execute("INSERT INTO workspace_journal (host_id, revision, command_id, payload_json, observed_at) VALUES (?1, NULL, ?2, ?3, ?4)", params![host, command, event.to_string(), now])?;
        tx.commit()?;
        Ok(())
    }).await
}

fn require_phase(value: &Value, command_id: &str, phase: &str) -> Result<(), HubError> {
    if value["commandId"] != command_id || value["phase"] != phase {
        return Err(HubError::BadRequest(format!(
            "Node did not acknowledge workspace command {command_id} as {phase}; refresh workspaces before retrying"
        )));
    }
    Ok(())
}

pub async fn observe_inventory(
    state: &AppState,
    host_id: &str,
    params: &Value,
) -> Result<(), HubError> {
    let inventory = params.get("host").unwrap_or(params);
    if inventory.get("workspaces").is_some() {
        observe_snapshot(state, host_id, inventory.clone(), None).await?;
    }
    Ok(())
}

async fn observe_snapshot(
    state: &AppState,
    host_id: &str,
    snapshot: Value,
    command_id: Option<String>,
) -> Result<(), HubError> {
    let revision = snapshot["workspaceRevision"]
        .as_u64()
        .and_then(|revision| i64::try_from(revision).ok())
        .ok_or_else(|| {
            HubError::BadRequest("Node workspaceRevision is missing or invalid".into())
        })?;
    let mut workspaces = snapshot["workspaces"]
        .as_array()
        .cloned()
        .ok_or_else(|| HubError::BadRequest("Node workspace list is missing".into()))?;
    for workspace in &mut workspaces {
        if !workspace["workspaceId"].is_string() || !workspace["root"].is_string() {
            return Err(HubError::BadRequest(
                "Node workspace record is invalid".into(),
            ));
        }
        workspace["hostId"] = json!(host_id);
    }
    let host = host_id.to_owned();
    let event = state.store.run_named("observe_snapshot", move |conn| {
        let tx = conn.transaction()?;
        let (old_revision, old_workspaces) = load_snapshot(&tx, &host)?;
        let changed = revision > old_revision;
        if revision == old_revision && old_workspaces != workspaces {
            return Err(StoreError::Id("Node changed workspaces without advancing workspaceRevision".into()));
        }
        let event = if changed {
            let now = crate::config::now_rfc3339();
            let snapshot = json!({"workspaceRevision": revision, "workspaces": workspaces});
            tx.execute(
                "INSERT INTO host_workspaces (host_id, revision, snapshot_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(host_id) DO UPDATE SET revision = excluded.revision, snapshot_json = excluded.snapshot_json",
                params![host, revision, snapshot.to_string()],
            )?;
            let event = json!({"type": "host.updated", "hostId": host, "workspaceRevision": revision,
                "workspaces": workspaces, "commandId": command_id, "observedAt": now});
            tx.execute(
                "INSERT INTO workspace_journal (host_id, revision, command_id, payload_json, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![host, revision, command_id, event.to_string(), now],
            )?;
            Some(event)
        } else {
            None
        };
        if let Some(command) = command_id {
            tx.execute(
                "UPDATE commands SET state = 'settled', resolution = 'clear', updated_at = ?1
                 WHERE id = ?2 AND host_id = ?3",
                params![crate::config::now_rfc3339(), command, host],
            )?;
        }
        tx.commit()?;
        Ok(event)
    }).await?;
    if let Some(event) = event {
        state
            .bus
            .publish(crate::ws::FollowEvent::json("", revision, event));
    }
    Ok(())
}

async fn snapshot_view(store: &Store, id: String) -> Result<Value, HubError> {
    let (revision, workspaces) = store
        .run_named("snapshot_view", move |conn| load_snapshot(conn, &id))
        .await?;
    Ok(json!({"workspaceRevision": revision.max(0), "workspaces": workspaces}))
}

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS host_workspaces (
            host_id TEXT PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL,
            snapshot_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workspace_journal (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            host_id TEXT NOT NULL,
            revision INTEGER,
            command_id TEXT,
            payload_json TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            UNIQUE(host_id, revision)
         );",
    )?;
    Ok(())
}

pub fn load_snapshot(conn: &Connection, id: &str) -> Result<(i64, Vec<Value>), StoreError> {
    let row = conn
        .query_row(
            "SELECT revision, snapshot_json FROM host_workspaces WHERE host_id = ?1",
            [id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    match row {
        Some((revision, raw)) => {
            let value: Value = serde_json::from_str(&raw)?;
            Ok((
                revision,
                value["workspaces"].as_array().cloned().unwrap_or_default(),
            ))
        }
        None => Ok((-1, Vec::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settled_locks_prune_when_unique_but_not_while_a_waiter_holds_the_arc() {
        // Round 4 item 4: prune requires settled AND Arc::strong_count == 1,
        // so a waiter that cloned the Arc while queued cannot have its mutex
        // replaced out from under it.
        let key_of = |workspace: &str| format!("hst-prune4\u{1f}{workspace}");
        let contains = |workspace: &str| {
            unbind_locks()
                .lock()
                .unwrap()
                .contains_key(&key_of(workspace))
        };

        // A holds the mutex; B queues behind it (it clones the Arc before
        // parking on lock_owned).
        let a = hold_workspace_operation("hst-prune4", "wsp").await;
        let waiter = tokio::spawn(hold_workspace_operation("hst-prune4", "wsp"));
        // Let B run until it parks on the contested mutex.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // B is queued, A not settled: an unrelated acquire prunes nothing of
        // wsp (unsettled entries always stay).
        let _other = hold_workspace_operation("hst-prune4", "other").await;
        assert!(contains("wsp"));

        // A settles and releases; B acquires the SAME mutex.
        drop(a);
        let b = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("queued waiter acquires")
            .expect("waiter task");

        // B holds the (settled) entry; an unrelated acquire must not prune it
        // because the map is not the sole Arc holder (B cloned it).
        let _other2 = hold_workspace_operation("hst-prune4", "other2").await;
        assert!(
            contains("wsp"),
            "settled entry with a queued/holding waiter must survive prune"
        );

        // B settles: now the map uniquely owns the Arc and it becomes prunable.
        drop(b);
        let _other3 = hold_workspace_operation("hst-prune4", "other3").await;
        assert!(!contains("wsp"), "settled sole-owner entry must be pruned");
    }

    #[tokio::test]
    async fn unregister_guard_blocks_task_binding_for_the_whole_prepare_to_settle() {
        // Round 5 item 1: the unregister guard is held beyond the occupancy
        // query, through prepare→settle. A task binding that starts after the
        // occupancy query but before settle must wait, not bind immediately.
        let unregister = tokio::spawn(hold_workspace_operation("hst-r5", "wsp"));
        // Let unregister park holding the lock.
        let held = tokio::time::timeout(std::time::Duration::from_secs(2), unregister)
            .await
            .expect("unregister acquires")
            .expect("unregister task");

        // A binding attempt starts and must be queued (not bound).
        let bind_started = Arc::new(tokio::sync::Notify::new());
        let bind_finished = Arc::new(tokio::sync::Notify::new());
        let bind = {
            let started = bind_started.clone();
            let finished = bind_finished.clone();
            tokio::spawn(async move {
                let _g = hold_workspace_operation("hst-r5", "wsp").await;
                let _ = (started, finished);
            })
        };
        bind_started.notify_one();
        // Give the binding task time to park on the mutex.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // It is still waiting while unregister holds.
        assert!(!bind.is_finished(), "binding must wait for unregister");

        // Unregister settles; the binding acquires (and then releases).
        drop(held);
        bind_finished.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), bind)
            .await
            .expect("binding eventually acquires after unregister settles")
            .expect("bind task");
    }

    /// Minimal tables carrying only the columns the usage guard reads.
    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE instances (host_id TEXT, workspace_id TEXT, lifecycle TEXT);
             CREATE TABLE tasks (state TEXT, archived_at TEXT, doc_json TEXT);",
        )
        .unwrap();
        conn
    }

    fn insert_instance(conn: &Connection, workspace: &str, lifecycle: &str) {
        conn.execute(
            "INSERT INTO instances (host_id, workspace_id, lifecycle) VALUES ('hst_a', ?1, ?2)",
            params![workspace, lifecycle],
        )
        .unwrap();
    }

    fn insert_task(conn: &Connection, state: &str, archived: bool, host: &str, workspace: &str) {
        let doc = json!({"workspaceBinding": {"hostId": host, "workspaceId": workspace}});
        conn.execute(
            "INSERT INTO tasks (state, archived_at, doc_json) VALUES (?1, ?2, ?3)",
            params![
                state,
                if archived {
                    Some("2026-10-05T00:00:00Z")
                } else {
                    None::<&str>
                },
                doc.to_string()
            ],
        )
        .unwrap();
    }

    #[test]
    fn both_terminal_lifecycles_exited_and_failed_stop_blocking() {
        let conn = seeded_conn();
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        insert_instance(&conn, "wsp_x", "ready");
        insert_instance(&conn, "wsp_x", "starting");
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (2, 0)
        );
        // Clean process end frees the directory; its history row is kept.
        conn.execute(
            "UPDATE instances SET lifecycle = 'exited' WHERE workspace_id = 'wsp_x'",
            [],
        )
        .unwrap();
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        // A process that ended with an error is equally terminal (D-057 OA6):
        // failed is process-end evidence, not a turn-level error.
        insert_instance(&conn, "wsp_x", "failed");
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        // `closed` (defensive; never emitted by a current Node) is not a
        // terminal lifecycle and keeps blocking.
        insert_instance(&conn, "wsp_x", "closed");
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (1, 0)
        );
        // A live session on another workspace never counts.
        insert_instance(&conn, "wsp_other", "ready");
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (1, 0)
        );
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_other").unwrap(),
            (1, 0)
        );
    }

    #[test]
    fn node_occupancy_refusals_are_conflicts_other_node_errors_stay_400() {
        assert!(
            node_unregister_conflict(
                "workspace /srv/app is still used by 2 live session(s); end them"
            )
            .is_some()
        );
        assert!(
            node_unregister_conflict(
                "workspace /srv/app is being unregistered; wait for it to settle"
            )
            .is_some()
        );
        assert!(
            node_unregister_conflict("workspace was replaced after unregister prepare").is_some()
        );
        assert!(
            node_unregister_conflict("workspace /outside is outside allowed workspace_roots")
                .is_none()
        );
        assert!(node_unregister_conflict("workspace /srv/app is not registered").is_none());
    }

    #[test]
    fn normalizes_absolute_aliases_and_rejects_relative() {
        assert_eq!(
            normalize_absolute("/srv/app/./src/../app").as_deref(),
            Some("/srv/app/app")
        );
        assert_eq!(
            normalize_absolute("/srv//app/").as_deref(),
            Some("/srv/app")
        );
        assert_eq!(normalize_absolute("/..").as_deref(), Some("/"));
        assert_eq!(normalize_absolute("/").as_deref(), Some("/"));
        assert_eq!(normalize_absolute("srv/app"), None);
        assert_eq!(normalize_absolute("../srv"), None);
    }

    #[test]
    fn active_tasks_block_but_done_failed_and_archived_tasks_do_not() {
        let conn = seeded_conn();
        for state in [
            "pending", "placed", "running", "stalled", "parked", "deferred",
        ] {
            insert_task(&conn, state, false, "hst_a", "wsp_x");
        }
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 6)
        );
        // Settled states no longer occupy the directory.
        conn.execute("UPDATE tasks SET state = 'done'", []).unwrap();
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        conn.execute("UPDATE tasks SET state = 'running'", [])
            .unwrap();
        // An archived task of any state is settled.
        conn.execute("UPDATE tasks SET archived_at = '2026-10-05T00:00:00Z'", [])
            .unwrap();
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        // Bindings on other host/workspace pairs never match.
        let fresh = seeded_conn();
        insert_task(&fresh, "running", false, "hst_b", "wsp_x");
        insert_task(&fresh, "running", false, "hst_a", "wsp_y");
        assert_eq!(
            count_workspace_users(&fresh, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
    }

    /// Records every RPC the sweep forwards; answers unregister aborts as the
    /// Node would once it released the mark (phase "aborted", membership
    /// intact).
    #[derive(Default)]
    struct RecordingAbortNode {
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl crate::transport::NodeTransport for RecordingAbortNode {
        fn kind(&self) -> crate::transport::TransportKind {
            crate::transport::TransportKind::SshStdio
        }

        fn call(
            &self,
            method: &str,
            params: Value,
            _timeout: std::time::Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<Value>, HubError>> + Send + '_>,
        > {
            let command_id = params
                .get("commandId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), command_id));
            Box::pin(async move { Ok(Some(json!({"phase": "aborted"}))) })
        }

        fn notify(
            &self,
            _method: &str,
            _params: Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, HubError>> + Send + '_>>
        {
            Box::pin(async { Ok(true) })
        }
    }

    /// r9 item 2: the post-reconnect sweep must abort only DELETEs a dead
    /// previous link left behind. A command created on the NEW link — a DELETE
    /// whose prepare/commit are in flight while the post-hello sweep runs —
    /// must never be aborted.
    #[tokio::test]
    async fn reconnect_sweep_skips_unregisters_created_on_the_new_link() {
        use crate::HubConfig;

        let dir = tempfile::tempdir().unwrap();
        let hub = crate::spawn(HubConfig::for_test(dir.path().join("data")))
            .await
            .unwrap();
        let state = hub.state.clone();
        let store = state.store.clone();
        let host = crate::config::new_id("hst").unwrap();
        hub.test_insert_host(&host).await.unwrap();

        let (old, _) = store
            .queue_command(
                None,
                None,
                host.clone(),
                "workspace.unregister".into(),
                json!({"path": "/srv/old", "workspaceId": "wsp_old"}),
                None,
            )
            .await
            .unwrap();
        let (new, _) = store
            .queue_command(
                None,
                None,
                host.clone(),
                "workspace.unregister".into(),
                json!({"path": "/srv/new", "workspaceId": "wsp_new"}),
                None,
            )
            .await
            .unwrap();
        // The old command belongs to the dead previous link (created before
        // the hello); the NEW one is post-dated to model a DELETE accepted on
        // the new link while the post-hello sweep is running.
        let new_id = new.command_id.clone();
        store
            .run_named("test_postdate_new_command", move |conn| {
                conn.execute(
                    "UPDATE commands SET created_at = '2099-01-01T00:00:00.000Z' WHERE id = ?1",
                    params![new_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let node = Arc::new(RecordingAbortNode::default());
        hub.test_set_node_transport(&host, node.clone()).await;

        abort_unsettled_unregisters_on_reconnect(&state, &host, &crate::config::now_rfc3339())
            .await
            .unwrap();

        // Exactly one abort, for the OLD command: the in-flight new-link
        // DELETE is untouched.
        let calls = node.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, "workspace.unregister");
        assert_eq!(calls[0].1, old.command_id);
        assert_ne!(calls[0].1, new.command_id);

        let old_row = store
            .get_command(old.command_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_row.state, "settled");
        assert_eq!(old_row.settlement_outcome.as_deref(), Some("rejected"));
        let new_row = store
            .get_command(new.command_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            new_row.state, "queued",
            "a DELETE accepted on the new link must not be aborted by the sweep"
        );

        // The cutoff is strict: a cutoff before both rows selects nothing.
        let before_both = store
            .list_unsettled_workspace_unregisters(&host, "2000-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert!(before_both.is_empty(), "{before_both:?}");
        // A cutoff after both timestamps still returns only the new row: the
        // old one is settled now and filtered by state.
        let later = store
            .list_unsettled_workspace_unregisters(&host, "2100-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(later.len(), 1, "{later:?}");
        assert_eq!(later[0].command_id, new.command_id);

        hub.shutdown().await;
    }
}
