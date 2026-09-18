//! Task ledger, path-ownership registry and placement ledger; design §2.2,
//! §2.5 (`parentTaskId` mirrors the recursive delegation tree), §4.3
//! (`scopeCheck.diffMustStayWithin: owns`), §5.6, §7 #8 (I1: dependency edges
//! unlock only from a landed sha, never from a worker status bit).
//!
//! Storage is three additive tables created in one self-contained migration
//! block; the task document itself is JSON like `projects`.

use crate::AppState;
use crate::agent_scope::{caller, caller_project_scope, require_grant};
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{Store, StoreError};
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_protocol::{
    PlacementLedgerRow, PlacementRejection, Task, TaskBudget, TaskClass, TaskDep, TaskId,
    TaskState, check_paths_within_owns, normalize_globs, parse_diff_paths,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

// ─── Request bodies ────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DepBody {
    task_id: TaskId,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BudgetBody {
    #[serde(default)]
    max_usd: Option<f64>,
    #[serde(default)]
    max_turns: Option<i64>,
    #[serde(default)]
    max_wall_mins: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTaskBody {
    project_id: remuda_protocol::ProjectId,
    title: String,
    intent: String,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    owns: Option<Vec<String>>,
    #[serde(default)]
    deps: Option<Vec<DepBody>>,
    #[serde(default)]
    budget: Option<BudgetBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SplitTaskBody {
    title: String,
    intent: String,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    owns: Option<Vec<String>>,
    #[serde(default)]
    deps: Option<Vec<DepBody>>,
    #[serde(default)]
    budget: Option<BudgetBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateBody {
    state: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandBody {
    sha: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimBody {
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiffCheckBody {
    task_id: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    diff: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlacementBody {
    kind: String,
    #[serde(default)]
    host_id: Option<remuda_protocol::HostId>,
    #[serde(default)]
    instance_id: Option<remuda_protocol::InstanceId>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    reasons: Option<Vec<String>>,
    #[serde(default)]
    rejected: Option<Vec<PlacementRejection>>,
}

/// One aggregated ownership-map row returned by `GET /v1/own`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnClaimRow {
    project_id: remuda_protocol::ProjectId,
    task_id: TaskId,
    paths: Vec<String>,
    claimed_by: String,
    claimed_at: remuda_protocol::Timestamp,
}

// ─── Migration ─────────────────────────────────────────────────────────────

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // One additive block for batch 4 (co-task): task ledger, the path
    // ownership registry, and placement ledger rows.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            parent_task_id TEXT,
            title TEXT NOT NULL,
            state TEXT NOT NULL,
            landed_sha TEXT,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
        CREATE INDEX IF NOT EXISTS tasks_project ON tasks(project_id);
        CREATE INDEX IF NOT EXISTS tasks_parent ON tasks(parent_task_id);
        CREATE TABLE IF NOT EXISTS task_paths (
            task_id TEXT NOT NULL,
            pattern TEXT NOT NULL,
            claimed_by TEXT NOT NULL DEFAULT '',
            claimed_at TEXT NOT NULL,
            PRIMARY KEY (task_id, pattern)
         );
        CREATE INDEX IF NOT EXISTS task_paths_pattern ON task_paths(pattern);
        CREATE TABLE IF NOT EXISTS placements (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
         );
        CREATE INDEX IF NOT EXISTS placements_task ON placements(task_id);
        CREATE INDEX IF NOT EXISTS placements_project ON placements(project_id);",
    )?;
    Ok(())
}

/// Routes for `/v1/tasks` and `/v1/own`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(get_task_http).patch(set_task_state))
        .route("/v1/tasks/{id}/split", post(split_task))
        .route("/v1/tasks/{id}/land", post(land_task))
        .route("/v1/tasks/{id}/own", post(claim_own).delete(release_own))
        .route(
            "/v1/tasks/{id}/placements",
            get(list_placements).post(add_placement),
        )
        .route("/v1/own", get(list_own))
        .route("/v1/own/check", post(check_own))
}

// ─── Authorization helpers ────────────────────────────────────────────────

async fn require_dispatch(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::store::Device, HubError> {
    let device = caller(state, headers).await?;
    require_grant(state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    Ok(device)
}

async fn require_project_scope(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(crate::store::Device, remuda_protocol::InstanceScope), HubError> {
    let device = caller(state, headers).await?;
    let scope = caller_project_scope(state, &device).await?;
    Ok((device, scope))
}

// ─── Task CRUD ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// `GET /v1/tasks?project=…&state=…`
async fn list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let state_filter = match query.state.as_deref() {
        None => None,
        Some(raw) => Some(parse_state(raw)?),
    };
    let mut items = state
        .store
        .list_tasks(query.project.clone())
        .await
        .map_err(map_store)?;
    items.retain(|task| scope.allows_project(task.project_id.as_id().as_str()));
    if let Some(wanted) = state_filter {
        items.retain(|task| task.state == wanted);
    }
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `POST /v1/tasks` — `remuda task add`.
async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateTaskBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    let (_, scope) = require_project_scope(&state, &headers).await?;
    if !scope.allows_project(body.project_id.as_id().as_str()) {
        return Err(HubError::Forbidden);
    }
    if body.title.trim().is_empty() || body.intent.trim().is_empty() {
        return Err(HubError::BadRequest("title and intent are required".into()));
    }
    let now = now_timestamp()?;
    let deps = resolve_deps(body.deps);
    let budget = budget_from(body.budget);
    let class = class_from(body.class.as_deref())?;
    let task = state
        .store
        .create_task(
            body.project_id.clone(),
            move |project, conn| {
                ensure_project_exists(conn, project)?;
                let task = Task::new_root(
                    now,
                    project.clone(),
                    body.title.trim().to_string(),
                    body.intent.trim().to_string(),
                    class,
                    body.owns.unwrap_or_default(),
                    deps,
                    budget,
                );
                ensure_deps_in_project(conn, &task)?;
                Ok(task)
            },
            device.id.clone(),
        )
        .await
        .map_err(map_store)?;
    state
        .store
        .append_audit(
            device.id,
            "task.add".into(),
            Some(task.meta.id.as_id().to_string()),
            json!({"projectId": task.project_id, "title": task.title}),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!(task)))
}

/// `GET /v1/tasks/{id}` — `remuda task show`.
async fn get_task_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let task = load_scoped_task(&state, &scope, &id).await?;
    // The live dependency view: edges locked until each dep carries a landed
    // sha (§7 #8). Computed at read time, never stored.
    let all = state.store.list_tasks(None).await.map_err(map_store)?;
    let locked = task.locked_deps(&all);
    let mut value = json!(task);
    value["lockedDeps"] = json!(locked.into_iter().map(|dep| json!(dep)).collect::<Vec<_>>());
    Ok(Json(value))
}

/// `PATCH /v1/tasks/{id}` — `remuda task set-state`.
async fn set_task_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<StateBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let target = parse_state(&body.state)?;
    let reason = body.reason.clone();
    let updated = state
        .store
        .mutate_task(id.clone(), move |task, conn| {
            if !scope.allows_project(task.project_id.as_id().as_str()) {
                return Err(StoreError::Forbidden("project outside scope".into()));
            }
            if !task.state.can_transition_to(target) {
                return Err(StoreError::Conflict(format!(
                    "illegal task transition {:?} → {:?}",
                    task.state, target
                )));
            }
            task.state = target;
            if matches!(target, TaskState::Failed) {
                task.blocked_reason = reason.clone();
            }
            if target.is_terminal() {
                release_all_claims(conn, &task.meta.id.as_id().to_string())?;
            }
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "task.state".into(),
            Some(id),
            json!({ "state": body.state, "reason": body.reason }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!(updated)))
}

/// `POST /v1/tasks/{id}/split` — `remuda task split`.
async fn split_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SplitTaskBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    let (_, scope) = require_project_scope(&state, &headers).await?;
    if body.title.trim().is_empty() || body.intent.trim().is_empty() {
        return Err(HubError::BadRequest("title and intent are required".into()));
    }
    let now = now_timestamp()?;
    let deps = resolve_deps(body.deps);
    let budget = budget_from(body.budget);
    let class = class_from(body.class.as_deref())?;
    let child = state
        .store
        .split_task(
            id.clone(),
            move |parent, conn| {
                if !scope.allows_project(parent.project_id.as_id().as_str()) {
                    return Err(StoreError::Forbidden("project outside scope".into()));
                }
                let child_depth = parent.depth() + 1;
                if let Some(project) =
                    load_project_doc(conn, &parent.project_id.as_id().to_string())?
                    && child_depth > project.policy.configurable.max_delegation_depth as u32
                {
                    return Err(StoreError::Conflict(format!(
                        "task delegation depth {child_depth} exceeds project maxDelegationDepth {}",
                        project.policy.configurable.max_delegation_depth
                    )));
                }
                let child = parent.new_child(
                    now,
                    body.title.trim().to_string(),
                    body.intent.trim().to_string(),
                    class,
                    body.owns.unwrap_or_default(),
                    deps,
                    budget,
                );
                ensure_deps_in_project(conn, &child)?;
                Ok(child)
            },
            device.id.clone(),
        )
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "task.split".into(),
            Some(child.meta.id.as_id().to_string()),
            json!({"parentTaskId": id, "title": child.title}),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!(child)))
}

/// `POST /v1/tasks/{id}/land` — record the landed sha from the gate/land step.
///
/// No CLI verb yet (batch 6 owns `remuda land`); this is the ledger entry the
/// gate calls, and the only thing dependency edges unlock from (§7 #8).
async fn land_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<LandBody>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Land).await?;
    let sha = body.sha.trim().to_string();
    if !(7..=40).contains(&sha.len()) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HubError::BadRequest(
            "landed sha must be 7–40 hex digits".into(),
        ));
    }
    let updated = state
        .store
        .mutate_task(id.clone(), move |task, conn| {
            if !matches!(task.state, TaskState::Running | TaskState::Done) {
                return Err(StoreError::Conflict(format!(
                    "can only land a running/done task (state {:?})",
                    task.state
                )));
            }
            task.landed_sha = Some(sha.clone());
            task.state = TaskState::Done;
            task.blocked_reason = None;
            release_all_claims(conn, &task.meta.id.as_id().to_string())?;
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "task.land".into(),
            Some(id),
            json!({ "landedSha": updated.landed_sha }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!(updated)))
}

// ─── Path ownership ────────────────────────────────────────────────────────

/// `POST /v1/tasks/{id}/own` — `remuda own claim`.
async fn claim_own(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ClaimBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    let patterns = normalize_globs(body.paths.clone());
    if patterns.is_empty() {
        return Err(HubError::BadRequest(
            "own claim requires at least one path".into(),
        ));
    }
    let device_id = device.id.clone();
    let audit_paths = patterns.clone();
    let updated = state
        .store
        .mutate_task(id.clone(), move |task, conn| {
            if task.state.is_terminal() {
                return Err(StoreError::Conflict(
                    "terminal tasks cannot claim paths".into(),
                ));
            }
            // Other active tasks' claims (rows are released on terminal state).
            let held: Vec<(String, String)> = {
                let mut stmt =
                    conn.prepare("SELECT pattern, task_id FROM task_paths WHERE task_id != ?1")?;
                let rows = stmt.query_map(params![task.meta.id.as_id().to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut conflicts = Vec::new();
            for new in &patterns {
                for (held_pattern, holder) in &held {
                    if remuda_protocol::patterns_conflict(new, held_pattern) {
                        conflicts.push(json!({
                            "path": new,
                            "heldBy": holder,
                            "heldPattern": held_pattern,
                        }));
                    }
                }
            }
            if !conflicts.is_empty() {
                return Err(StoreError::Conflict(format!(
                    "ownership claim conflicts: {}",
                    conflicts
                        .iter()
                        .map(|value| value["heldPattern"].as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let now = crate::config::now_rfc3339();
            for pattern in &patterns {
                conn.execute(
                    "INSERT INTO task_paths (task_id, pattern, claimed_by, claimed_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (task_id, pattern) DO UPDATE SET claimed_by = ?3, claimed_at = ?4",
                    params![task.meta.id.as_id().to_string(), pattern, device_id, now],
                )?;
                if !task.owns.iter().any(|owned| owned == pattern) {
                    task.owns.push(pattern.clone());
                }
            }
            task.owns = normalize_globs(std::mem::take(&mut task.owns));
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "own.claim".into(),
            Some(id),
            json!({ "paths": audit_paths }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(
        json!({ "taskId": updated.meta.id, "owns": updated.owns }),
    ))
}

/// `DELETE /v1/tasks/{id}/own` — `remuda own release`.
async fn release_own(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ClaimBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    let patterns = normalize_globs(body.paths.clone());
    let audit_paths = patterns.clone();
    let updated = state
        .store
        .mutate_task(id.clone(), move |task, conn| {
            if patterns.is_empty() {
                release_all_claims(conn, &task.meta.id.as_id().to_string())?;
                task.owns.clear();
            } else {
                for pattern in &patterns {
                    conn.execute(
                        "DELETE FROM task_paths WHERE task_id = ?1 AND pattern = ?2",
                        params![task.meta.id.as_id().to_string(), pattern],
                    )?;
                }
                task.owns.retain(|owned| !patterns.contains(owned));
            }
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "own.release".into(),
            Some(id),
            json!({ "paths": audit_paths }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(
        json!({ "taskId": updated.meta.id, "owns": updated.owns }),
    ))
}

/// `GET /v1/own?project=…` — active ownership claims (the ownership map).
async fn list_own(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let rows = state
        .store
        .list_own_claims(query.project.clone())
        .await
        .map_err(map_store)?;
    let rows: Vec<OwnClaimRow> = rows
        .into_iter()
        .filter(|row| scope.allows_project(row.project_id.as_id().as_str()))
        .collect();
    Ok(Json(json!({ "items": rows })))
}

/// `POST /v1/own/check` — `remuda own check <diff>`; design §4.3/§7 #3.
///
/// Pure path reconciliation against the task's `owns[]`; semantic diff review
/// remains the LLM gate's job.
async fn check_own(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DiffCheckBody>,
) -> Result<Json<Value>, HubError> {
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let task = state
        .store
        .get_task(body.task_id.clone())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if !scope.allows_project(task.project_id.as_id().as_str()) {
        return Err(HubError::Forbidden);
    }
    let changed = match (body.paths.as_ref(), body.diff.as_deref()) {
        (Some(paths), _) => normalize_globs(paths.clone()),
        (None, Some(diff)) => parse_diff_paths(diff),
        (None, None) => {
            return Err(HubError::BadRequest(
                "own check requires `paths` or `diff`".into(),
            ));
        }
    };
    let check = check_paths_within_owns(&task.owns, &changed);
    Ok(Json(json!({
        "taskId": task.meta.id,
        "state": task.state,
        "within": check.within,
        "checked": check.checked,
        "violations": check.violations,
    })))
}

// ─── Placement ledger ──────────────────────────────────────────────────────

/// `GET /v1/tasks/{id}/placements`
async fn list_placements(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let _task = load_scoped_task(&state, &scope, &id).await?;
    let items = state
        .store
        .list_placements(id.clone())
        .await
        .map_err(map_store)?;
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `POST /v1/tasks/{id}/placements` — append a placement ledger row.
///
/// The row drives the legal task-state transition (`dispatch`→placed,
/// `park`→parked, `unplace`→pending, `switch-model` leaves state) and updates
/// the task's placement reference. `reasons[]`/`rejected[]` are the audit
/// trail and the future bot card in one representation (§5.6).
async fn add_placement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PlacementBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    let (_, scope) = require_project_scope(&state, &headers).await?;
    let kind = body.kind.clone();
    let target_state = match body.kind.as_str() {
        remuda_protocol::PLACEMENT_KIND_DISPATCH => Some(TaskState::Placed),
        remuda_protocol::PLACEMENT_KIND_PARK => Some(TaskState::Parked),
        remuda_protocol::PLACEMENT_KIND_UNPLACE => Some(TaskState::Pending),
        remuda_protocol::PLACEMENT_KIND_SWITCH_MODEL => None,
        other => {
            return Err(HubError::BadRequest(format!(
                "unknown placement kind {other:?}"
            )));
        }
    };
    let row_and_task = state
        .store
        .append_placement(
            id.clone(),
            body,
            move |task, _conn, row| {
                if !scope.allows_project(task.project_id.as_id().as_str()) {
                    return Err(StoreError::Forbidden("project outside scope".into()));
                }
                // A model switch is an explicit act on a *running* worker
                // (design §4.6) — it never implicitly places a pending task.
                if kind == remuda_protocol::PLACEMENT_KIND_SWITCH_MODEL
                    && task.state != TaskState::Running
                {
                    return Err(StoreError::Conflict(format!(
                        "switch-model requires a running task (state {:?})",
                        task.state
                    )));
                }
                if let Some(target) = target_state {
                    if !task.state.can_transition_to(target) {
                        return Err(StoreError::Conflict(format!(
                            "placement {} requires {:?} from {:?}",
                            row.kind, target, task.state
                        )));
                    }
                    task.state = target;
                }
                match row.kind.as_str() {
                    remuda_protocol::PLACEMENT_KIND_UNPLACE => task.placement = None,
                    remuda_protocol::PLACEMENT_KIND_SWITCH_MODEL => {
                        if let Some(slot) = task.placement.as_mut() {
                            slot.placement_id = Some(row.id.clone());
                            slot.model = row.model.clone();
                            if slot.host_id.is_none() {
                                slot.host_id = row.host_id.clone();
                            }
                            if slot.instance_id.is_none() {
                                slot.instance_id = row.instance_id.clone();
                            }
                            if slot.branch.is_none() {
                                slot.branch = row.branch.clone();
                            }
                        } else {
                            task.placement = Some(placement_ref(row));
                        }
                    }
                    _ => task.placement = Some(placement_ref(row)),
                }
                Ok(())
            },
            device.id.clone(),
        )
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "placement.add".into(),
            Some(id),
            json!({
                "kind": row_and_task.0.kind,
                "hostId": row_and_task.0.host_id,
                "model": row_and_task.0.model,
                "reasons": row_and_task.0.reasons.len(),
                "rejected": row_and_task.0.rejected.len(),
            }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(
        json!({ "placement": row_and_task.0, "task": row_and_task.1 }),
    ))
}

fn placement_ref(row: &PlacementLedgerRow) -> remuda_protocol::TaskPlacementRef {
    remuda_protocol::TaskPlacementRef {
        placement_id: Some(row.id.clone()),
        host_id: row.host_id.clone(),
        instance_id: row.instance_id.clone(),
        branch: row.branch.clone(),
        model: row.model.clone(),
    }
}

// ─── Shared helpers ────────────────────────────────────────────────────────

async fn load_scoped_task(
    state: &AppState,
    scope: &remuda_protocol::InstanceScope,
    id: &str,
) -> Result<Task, HubError> {
    let task = state
        .store
        .get_task(id.to_string())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if !scope.allows_project(task.project_id.as_id().as_str()) {
        return Err(HubError::Forbidden);
    }
    Ok(task)
}

fn now_timestamp() -> Result<remuda_protocol::Timestamp, HubError> {
    remuda_protocol::Timestamp::try_from(crate::config::now_rfc3339())
        .map_err(|err| HubError::Internal(format!("clock produced a bad timestamp: {err}")))
}

fn parse_state(raw: &str) -> Result<TaskState, HubError> {
    serde_json::from_value(json!(raw))
        .map_err(|_| HubError::BadRequest(format!("unknown task state {raw:?}")))
}

fn class_from(raw: Option<&str>) -> Result<TaskClass, HubError> {
    match raw {
        None => Ok(TaskClass::Implement),
        Some(value) => serde_json::from_value(json!(value))
            .map_err(|_| HubError::BadRequest(format!("unknown task class {value:?}"))),
    }
}

fn resolve_deps(deps: Option<Vec<DepBody>>) -> Vec<TaskDep> {
    deps.unwrap_or_default()
        .into_iter()
        .map(|dep| TaskDep {
            task_id: dep.task_id,
            note: dep.note,
        })
        .collect()
}

fn budget_from(body: Option<BudgetBody>) -> TaskBudget {
    match body {
        Some(body) => TaskBudget {
            max_usd: body.max_usd,
            max_turns: body.max_turns,
            max_wall_mins: body.max_wall_mins,
        },
        None => TaskBudget::default(),
    }
}

fn ensure_project_exists(
    conn: &Connection,
    project_id: &remuda_protocol::ProjectId,
) -> Result<(), StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
        params![project_id.as_id().to_string()],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(StoreError::Id(format!(
            "unknown project {}",
            project_id.as_id()
        )))
    }
}

fn load_project_doc(
    conn: &Connection,
    project_id: &str,
) -> Result<Option<remuda_protocol::Project>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(StoreError::from)
}

/// Every dep must reference an existing task in the same project; design keeps
/// the task ledger per-project.
fn ensure_deps_in_project(conn: &Connection, task: &Task) -> Result<(), StoreError> {
    for dep in &task.deps {
        if dep.task_id == task.meta.id {
            return Err(StoreError::Id("a task cannot depend on itself".into()));
        }
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT project_id, state FROM tasks WHERE id = ?1",
                params![dep.task_id.as_id().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project_id, _state)) = row else {
            return Err(StoreError::Id(format!(
                "unknown dependency task {}",
                dep.task_id.as_id()
            )));
        };
        if project_id != task.project_id.as_id().to_string() {
            return Err(StoreError::Id(format!(
                "dependency {} is in another project",
                dep.task_id.as_id()
            )));
        }
    }
    Ok(())
}

fn release_all_claims(conn: &Connection, task_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM task_paths WHERE task_id = ?1",
        params![task_id],
    )?;
    Ok(())
}

// ─── Store-backed CRUD ─────────────────────────────────────────────────────

impl Store {
    /// Insert a root task built inside `build` (same writer transaction, so
    /// project/dep checks see committed rows).
    pub async fn create_task<F>(
        &self,
        project_id: remuda_protocol::ProjectId,
        build: F,
        created_by: String,
    ) -> Result<Task, StoreError>
    where
        F: FnOnce(&remuda_protocol::ProjectId, &mut Connection) -> Result<Task, StoreError>
            + Send
            + 'static,
    {
        self.run_named("create_task", move |conn| {
            let task = build(&project_id, conn)?;
            insert_task_row(conn, &task, &created_by)?;
            Ok(task)
        })
        .await
    }

    /// Build and insert a child task under `parent_id`.
    pub async fn split_task<F>(
        &self,
        parent_id: String,
        build: F,
        created_by: String,
    ) -> Result<Option<Task>, StoreError>
    where
        F: FnOnce(&Task, &mut Connection) -> Result<Task, StoreError> + Send + 'static,
    {
        self.run_named("split_task", move |conn| {
            let Some(parent) = load_task(conn, &parent_id)? else {
                return Ok(None);
            };
            let child = build(&parent, conn)?;
            if child.parent_task_id.as_ref() != Some(&parent.meta.id) {
                return Err(StoreError::Id("split produced a mismatched parent".into()));
            }
            insert_task_row(conn, &child, &created_by)?;
            Ok(Some(child))
        })
        .await
    }

    /// Apply a mutation to a task doc; the closure may also touch the registry
    /// tables in the same writer turn. Bumps revision + updated_at.
    pub async fn mutate_task<F>(
        &self,
        task_id: String,
        mutate: F,
    ) -> Result<Option<Task>, StoreError>
    where
        F: FnOnce(&mut Task, &mut Connection) -> Result<(), StoreError> + Send + 'static,
    {
        self.run_named("mutate_task", move |conn| {
            let Some(mut task) = load_task(conn, &task_id)? else {
                return Ok(None);
            };
            mutate(&mut task, conn)?;
            bump_revision(&mut task)?;
            update_task_row(conn, &task)?;
            Ok(Some(task))
        })
        .await
    }

    /// Append a placement row and let `apply` mutate the task doc in the same
    /// writer turn. Returns `(row, task)` or `None` when the task is missing.
    async fn append_placement<F>(
        &self,
        task_id: String,
        body: PlacementBody,
        apply: F,
        created_by: String,
    ) -> Result<Option<(PlacementLedgerRow, Task)>, StoreError>
    where
        F: FnOnce(&mut Task, &mut Connection, &PlacementLedgerRow) -> Result<(), StoreError>
            + Send
            + 'static,
    {
        self.run_named("append_placement", move |conn| {
            let Some(mut task) = load_task(conn, &task_id)? else {
                return Ok(None);
            };
            let id = remuda_protocol::Id::new("plc")
                .map_err(|err| StoreError::Id(err.0))?;
            let now = crate::config::now_rfc3339();
            let row = PlacementLedgerRow {
                id,
                task_id: task.meta.id.clone(),
                project_id: task.project_id.clone(),
                kind: body.kind,
                host_id: body.host_id,
                instance_id: body.instance_id,
                harness: body.harness,
                model: body.model,
                branch: body.branch,
                reasons: body.reasons.unwrap_or_default(),
                rejected: body.rejected.unwrap_or_default(),
                created_by: created_by.clone(),
                created_at: remuda_protocol::Timestamp::try_from(now.clone())
                    .map_err(|err| StoreError::Id(format!("bad timestamp: {err}")))?,
            };
            apply(&mut task, conn, &row)?;
            bump_revision(&mut task)?;
            conn.execute(
                "INSERT INTO placements (id, task_id, project_id, kind, doc_json, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.id.to_string(),
                    row.task_id.as_id().to_string(),
                    row.project_id.as_id().to_string(),
                    row.kind,
                    serde_json::to_string(&row)?,
                    created_by,
                    now
                ],
            )?;
            update_task_row(conn, &task)?;
            Ok(Some((row, task)))
        })
        .await
    }

    /// One task.
    pub async fn get_task(&self, task_id: String) -> Result<Option<Task>, StoreError> {
        self.run_named("get_task", move |conn| load_task(conn, &task_id))
            .await
    }

    /// List tasks, optionally within one project, oldest first.
    pub async fn list_tasks(&self, project_id: Option<String>) -> Result<Vec<Task>, StoreError> {
        self.run_named("list_tasks", move |conn| {
            let ids: Vec<String> = match &project_id {
                Some(project) => {
                    let mut stmt = conn.prepare(
                        "SELECT id FROM tasks WHERE project_id = ?1 ORDER BY created_at",
                    )?;
                    let rows = stmt.query_map(params![project], |row| row.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                }
                None => {
                    let mut stmt = conn.prepare("SELECT id FROM tasks ORDER BY created_at")?;
                    let rows = stmt.query_map([], |row| row.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                }
            };
            ids.into_iter()
                .map(|id| {
                    load_task(conn, &id)?.ok_or_else(|| StoreError::Id("task vanished".into()))
                })
                .collect()
        })
        .await
    }

    /// Placement rows for one task, oldest first.
    pub async fn list_placements(
        &self,
        task_id: String,
    ) -> Result<Vec<PlacementLedgerRow>, StoreError> {
        self.run_named("list_placements", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT doc_json FROM placements WHERE task_id = ?1 ORDER BY created_at",
            )?;
            let docs: Vec<String> = stmt
                .query_map(params![task_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            docs.into_iter()
                .map(|doc| Ok(serde_json::from_str(&doc)?))
                .collect()
        })
        .await
    }

    /// Aggregate active path claims, one row per task.
    async fn list_own_claims(
        &self,
        project_id: Option<String>,
    ) -> Result<Vec<OwnClaimRow>, StoreError> {
        self.run_named("list_own_claims", move |conn| {
            // Registry rows only exist for non-terminal tasks (released on
            // terminal transition), so no state filter is needed.
            let mut stmt = conn.prepare(
                "SELECT t.project_id, p.task_id, p.pattern, p.claimed_by, p.claimed_at
                 FROM task_paths p JOIN tasks t ON t.id = p.task_id
                 WHERE (?1 IS NULL OR t.project_id = ?1)
                 ORDER BY p.task_id, p.pattern",
            )?;
            let rows: Vec<(String, String, String, String, String)> = stmt
                .query_map(params![project_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out: Vec<OwnClaimRow> = Vec::new();
            for (project, task, pattern, by, at) in rows {
                if let Some(last) = out.last_mut()
                    && last.task_id.as_id().as_str() == task
                {
                    last.paths.push(pattern);
                    continue;
                }
                out.push(OwnClaimRow {
                    project_id: remuda_protocol::ProjectId::try_from(project)
                        .map_err(|err| StoreError::Id(err.0))?,
                    task_id: TaskId::try_from(task).map_err(|err| StoreError::Id(err.0))?,
                    paths: vec![pattern],
                    claimed_by: by,
                    claimed_at: remuda_protocol::Timestamp::try_from(at)
                        .map_err(|err| StoreError::Id(err.0))?,
                });
            }
            Ok(out)
        })
        .await
    }
}

fn bump_revision(task: &mut Task) -> Result<(), StoreError> {
    let now = crate::config::now_rfc3339();
    task.meta.updated_at = remuda_protocol::Timestamp::try_from(now)
        .map_err(|err| StoreError::Id(format!("bad timestamp: {err}")))?;
    task.meta.revision = remuda_protocol::U64(task.meta.revision.0 + 1);
    Ok(())
}

fn insert_task_row(conn: &mut Connection, task: &Task, created_by: &str) -> Result<(), StoreError> {
    let now = String::from(task.meta.updated_at.clone());
    // `owns` supplied at add/split time are claims too: reject conflicts with
    // active tasks and seed the registry in the same writer turn.
    let mut stmt = conn.prepare("SELECT pattern FROM task_paths WHERE task_id != ?1")?;
    let held: Vec<String> = stmt
        .query_map(params![task.meta.id.as_id().to_string()], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for new in &task.owns {
        if held
            .iter()
            .any(|held_pattern| remuda_protocol::patterns_conflict(new, held_pattern))
        {
            return Err(StoreError::Conflict(format!(
                "ownership claim conflicts for {new} with an active task"
            )));
        }
    }
    conn.execute(
        "INSERT INTO tasks
            (id, project_id, parent_task_id, title, state, landed_sha, doc_json, revision, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?9)",
        params![
            task.meta.id.as_id().to_string(),
            task.project_id.as_id().to_string(),
            task.parent_task_id.as_ref().map(|id| id.as_id().to_string()),
            task.title,
            serde_json::to_value(task.state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            task.landed_sha,
            serde_json::to_string(task).expect("task json"),
            created_by,
            now
        ],
    )?;
    for pattern in &task.owns {
        conn.execute(
            "INSERT INTO task_paths (task_id, pattern, claimed_by, claimed_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![task.meta.id.as_id().to_string(), pattern, created_by, now],
        )?;
    }
    Ok(())
}

fn update_task_row(conn: &Connection, task: &Task) -> rusqlite::Result<()> {
    let now = String::from(task.meta.updated_at.clone());
    let state = serde_json::to_value(task.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    conn.execute(
        "UPDATE tasks SET title = ?1, state = ?2, landed_sha = ?3, doc_json = ?4,
                         revision = ?5, updated_at = ?6
         WHERE id = ?7",
        params![
            task.title,
            state,
            task.landed_sha,
            serde_json::to_string(task).expect("task json"),
            task.meta.revision.0 as i64,
            now,
            task.meta.id.as_id().to_string()
        ],
    )?;
    Ok(())
}

fn load_task(conn: &Connection, id: &str) -> Result<Option<Task>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM tasks WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(StoreError::from)
}
