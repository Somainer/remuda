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

/// Lifecycles that still count as using a workspace (same set the seat
/// uniqueness guard uses). Ended sessions (`exited`/`failed`/`closed`) keep
/// their history rows but never block an unregister.
const ACTIVE_WORKSPACE_SESSION_SQL: &str = "lifecycle NOT IN ('exited', 'failed', 'closed')";

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

/// Map an absolute root path to the workspace id in the host's last observed
/// snapshot, so the path-bodied DELETE route can run the usage guard.
async fn workspace_id_for_path(
    state: &AppState,
    host_id: &str,
    path: &str,
) -> Result<Option<String>, HubError> {
    let host = host_id.to_owned();
    let path = path.to_owned();
    let (_, workspaces) = state
        .store
        .run_named("workspace_id_for_path", move |conn| {
            load_snapshot(conn, &host)
        })
        .await?;
    Ok(workspaces
        .iter()
        .find(|workspace| workspace["root"].as_str() == Some(path.as_str()))
        .and_then(|workspace| workspace["workspaceId"].as_str().map(str::to_owned)))
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
    // task still uses. The check runs before a command is queued, so the
    // refusal leaves neither a command row nor a prepared Node mutation.
    if method == "workspace.unregister"
        && let Some(workspace_id) = workspace_id_for_path(state, &id, body.path.trim()).await?
    {
        let (sessions, tasks) = workspace_users(state, &id, &workspace_id).await?;
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
    }
    let mut payload = json!({"path": body.path});
    crate::agent_scope::stamp(&mut payload, &device);
    let (command, _) = state
        .store
        .queue_command(None, None, id.clone(), method.into(), payload, None)
        .await?;
    state
        .store
        .mark_forward_intent(command.command_id.clone())
        .await?;
    let prepared = crate::http::call_node(
        state,
        &id,
        method,
        json!({"path": body.path, "commandId": command.command_id, "phase": "prepare"}),
    )
    .await;
    let prepared = match prepared {
        Err(HubError::BadRequest(reason)) => {
            mark_rejected(&state.store, &command.command_id, &id, &reason).await?;
            return Err(HubError::BadRequest(reason));
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
    let settled = crate::http::call_node(
        state,
        &id,
        method,
        json!({"path": body.path, "commandId": command.command_id, "phase": "commit"}),
    )
    .await?;
    require_phase(&settled, &command.command_id, "settled")?;
    let workspace_id = settled.get("workspaceId").cloned();
    observe_snapshot(state, &id, settled, Some(command.command_id)).await?;
    let mut response = snapshot_view(&state.store, id).await?;
    if let Some(workspace_id) = workspace_id {
        response["workspaceId"] = workspace_id;
    }
    Ok(Json(response))
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
    fn live_sessions_block_but_ended_sessions_keep_history_without_blocking() {
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
        // Ended/failed sessions keep their rows (history) and stop blocking.
        conn.execute(
            "UPDATE instances SET lifecycle = 'exited' WHERE workspace_id = 'wsp_x'",
            [],
        )
        .unwrap();
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        insert_instance(&conn, "wsp_x", "failed");
        insert_instance(&conn, "wsp_x", "closed");
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        // A live session on another workspace never counts.
        insert_instance(&conn, "wsp_other", "ready");
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_x").unwrap(),
            (0, 0)
        );
        assert_eq!(
            count_workspace_users(&conn, "hst_a", "wsp_other").unwrap(),
            (1, 0)
        );
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
}
