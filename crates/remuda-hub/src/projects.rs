//! Thin authoritative Hub Project entity; design §3.1–§3.2.
//!
//! The Project document is the authority; the repo `.remuda/project.toml`
//! stays advisory. Members reuse the Space key `(hostId, workspaceId)` —
//! Project is what legitimately spans hosts, Space does not (D-024).

use crate::AppState;
use crate::agent_scope::{caller, caller_project_scope, require_grant, require_operator};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{Store, StoreError};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_protocol::{
    EntityMeta, GrantVerb, Project, ProjectGate, ProjectHostQuota, ProjectMember, ProjectPlacement,
    ProjectPolicy, ProjectProviderRef, U64,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // Skeleton for the coordinator batches: batch 2 adds card tickets, batch 4
    // adds task tables alongside this one; columns grow via `ensure_column`.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// Routes for `/v1/projects`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/{id}",
            get(get_project_http)
                .patch(set_project)
                .delete(delete_project),
        )
        .route(
            "/v1/projects/{id}/members",
            post(add_member).delete(remove_member),
        )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProjectBody {
    name: String,
    #[serde(default)]
    home_host: Option<String>,
    #[serde(default)]
    repo_remote: Option<String>,
    #[serde(default)]
    default_base_branch: Option<String>,
    #[serde(default)]
    branch_pattern: Option<String>,
    /// Seed members (`{hostId, workspaceId, role?}`); both hosts must be known.
    #[serde(default)]
    members: Vec<MemberBody>,
    /// Per-host capacity quotas (`{hostId, maxInstances, maxBuilding,
    /// diskBudgetGb, portBlocks, requires, latencyClass}`); the hosts must be
    /// known. Read by placement §3.4, hostcap and worker port allocation.
    #[serde(default)]
    hosts: Vec<ProjectHostQuota>,
    #[serde(default)]
    provider: Option<ProjectProviderRef>,
    #[serde(default)]
    default_effort: Option<String>,
    #[serde(default)]
    permission_posture: Option<String>,
    #[serde(default)]
    policy: Option<ProjectPolicy>,
    #[serde(default)]
    placement: Option<ProjectPlacement>,
    #[serde(default)]
    gate: Option<ProjectGate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemberBody {
    host_id: String,
    workspace_id: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchProjectBody {
    /// Full replacement fields the operator may set; absent = unchanged.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    home_host: Option<Option<String>>,
    #[serde(default)]
    repo_remote: Option<Option<String>>,
    #[serde(default)]
    default_base_branch: Option<String>,
    #[serde(default)]
    branch_pattern: Option<String>,
    #[serde(default)]
    provider: Option<ProjectProviderRef>,
    #[serde(default)]
    default_effort: Option<Option<String>>,
    #[serde(default)]
    permission_posture: Option<Option<String>>,
    #[serde(default)]
    policy: Option<ProjectPolicy>,
    #[serde(default)]
    placement: Option<ProjectPlacement>,
    #[serde(default)]
    gate: Option<ProjectGate>,
    /// Full replacement of per-host capacity quotas.
    #[serde(default)]
    hosts: Option<Vec<ProjectHostQuota>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveMemberBody {
    host_id: String,
    workspace_id: String,
}

/// Human/Bot, or an Agent holding `dispatch` whose scope covers the project.
/// Leaf workers (no grants) get 403 — design §2.4.
async fn require_coordinator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::store::Device, HubError> {
    let device = caller(state, headers).await?;
    require_grant(state, &device, GrantVerb::Dispatch).await?;
    Ok(device)
}

/// `GET /v1/projects`
async fn list_projects(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    let device = require_coordinator(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let mut items = state.store.list_projects().await.map_err(map_store)?;
    items.retain(|project| scope.allows_project(project.meta.id.as_id().as_str()));
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `POST /v1/projects`
async fn create_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateProjectBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_operator(&state, &headers).await?;
    if body.name.trim().is_empty() {
        return Err(HubError::BadRequest("project name required".into()));
    }
    if let Some(host) = &body.home_host {
        require_known_host(&state, host).await?;
    }
    let members = resolve_members(&state, &body.members).await?;
    let hosts = resolve_hosts(&state, body.hosts).await?;
    let now = crate::config::now_rfc3339();
    let id = remuda_protocol::ProjectId::new();
    let mut project = Project {
        meta: EntityMeta {
            id: id.clone(),
            revision: U64(1),
            created_at: parse_timestamp(&now)?,
            updated_at: parse_timestamp(&now)?,
        },
        name: body.name.trim().to_string(),
        home_host: body
            .home_host
            .map(parse_branded::<remuda_protocol::HostId>)
            .transpose()
            .map_err(|err| HubError::BadRequest(format!("homeHost: {err}")))?,
        repo_remote: body.repo_remote,
        default_base_branch: body.default_base_branch.unwrap_or_else(|| "main".into()),
        branch_pattern: body
            .branch_pattern
            .unwrap_or_else(|| "wt/{worker}/{topic}".into()),
        members,
        hosts,
        placement: body.placement.unwrap_or_default(),
        provider: body.provider.unwrap_or_default(),
        model_roles: Default::default(),
        default_effort: body.default_effort,
        permission_posture: body.permission_posture,
        gate: body.gate.unwrap_or_default(),
        policy: body.policy.unwrap_or_default(),
        brief_ref: ".remuda/brief.md".into(),
    };
    // Enforced policy is D-031: it can be set at creation by the operator but
    // never weakened by an agent later.
    project.policy.enforced = Default::default();
    let created = state
        .store
        .insert_project(project, device.id.clone())
        .await
        .map_err(map_store)?;
    state
        .store
        .append_audit(
            device.id,
            "project.create".into(),
            Some(created.meta.id.as_id().to_string()),
            json!({ "name": created.name }),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!(created)))
}

/// `GET /v1/projects/{id}`
async fn get_project_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let device = require_coordinator(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&id) {
        return Err(HubError::Forbidden);
    }
    let project = state
        .store
        .get_project(id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    Ok(Json(json!(project)))
}

/// `PATCH /v1/projects/{id}` — `remuda project set`.
async fn set_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchProjectBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_coordinator(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&id) {
        return Err(HubError::Forbidden);
    }
    if let Some(host) = body.home_host.as_ref().and_then(Option::as_ref) {
        require_known_host(&state, host).await?;
    }
    let hosts = if body.hosts.is_some() {
        Some(resolve_hosts(&state, body.hosts.unwrap_or_default()).await?)
    } else {
        None
    };
    let updated = state
        .store
        .patch_project(id.clone(), move |project| {
            if let Some(name) = body.name {
                if name.trim().is_empty() {
                    return Err(StoreError::Id("project name required".into()));
                }
                project.name = name.trim().to_string();
            }
            if let Some(home) = body.home_host {
                project.home_host = home
                    .map(|raw| {
                        remuda_protocol::HostId::try_from(raw)
                            .map_err(|err| StoreError::Id(format!("homeHost: {err}")))
                    })
                    .transpose()?;
            }
            if let Some(remote) = body.repo_remote {
                project.repo_remote = remote;
            }
            if let Some(branch) = body.default_base_branch {
                project.default_base_branch = branch;
            }
            if let Some(pattern) = body.branch_pattern {
                project.branch_pattern = pattern;
            }
            if let Some(provider) = body.provider {
                project.provider = provider;
            }
            if let Some(effort) = body.default_effort {
                project.default_effort = effort;
            }
            if let Some(posture) = body.permission_posture {
                project.permission_posture = posture;
            }
            if let Some(policy) = body.policy {
                // D-031: keep the stored enforced switches; only configurable
                // policy is mutable after creation.
                project.policy.configurable = policy.configurable;
            }
            if let Some(placement) = body.placement {
                project.placement = placement;
            }
            if let Some(gate) = body.gate {
                project.gate = gate;
            }
            if let Some(hosts) = hosts {
                project.hosts = hosts;
            }
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(device.id, "project.set".into(), Some(id), json!({}))
        .await
        .map_err(map_store)?;
    Ok(Json(json!(updated)))
}

/// `DELETE /v1/projects/{id}` — operator only; returns the removed document.
async fn delete_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_operator(&state, &headers).await?;
    let removed = state
        .store
        .delete_project(id.clone())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(device.id, "project.delete".into(), Some(id), json!({}))
        .await
        .map_err(map_store)?;
    Ok(Json(json!({ "deleted": true, "project": removed })))
}

/// `POST /v1/projects/{id}/members`
async fn add_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MemberBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_coordinator(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&id) {
        return Err(HubError::Forbidden);
    }
    let member = resolve_members(
        &state,
        &[MemberBody {
            host_id: body.host_id.clone(),
            workspace_id: body.workspace_id.clone(),
            role: body.role.clone(),
        }],
    )
    .await?
    .pop()
    .expect("one resolved member");
    let updated = state
        .store
        .patch_project(id.clone(), move |project| {
            if project.members.iter().any(|existing| {
                existing.host_id == member.host_id && existing.workspace_id == member.workspace_id
            }) {
                return Err(StoreError::Id("member already exists".into()));
            }
            project.members.push(member);
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(device.id, "project.member.add".into(), Some(id), json!({}))
        .await
        .map_err(map_store)?;
    Ok(Json(json!(updated)))
}

/// `DELETE /v1/projects/{id}/members`
async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RemoveMemberBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_coordinator(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&id) {
        return Err(HubError::Forbidden);
    }
    let host = parse_branded::<remuda_protocol::HostId>(body.host_id)
        .map_err(|err| HubError::BadRequest(format!("hostId: {err}")))?;
    let workspace = parse_branded::<remuda_protocol::WorkspaceId>(body.workspace_id)
        .map_err(|err| HubError::BadRequest(format!("workspaceId: {err}")))?;
    let updated = state
        .store
        .patch_project(id.clone(), move |project| {
            let before = project.members.len();
            project
                .members
                .retain(|member| !(member.host_id == host && member.workspace_id == workspace));
            if project.members.len() == before {
                return Err(StoreError::Id("member not found".into()));
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
            "project.member.remove".into(),
            Some(id),
            json!({}),
        )
        .await
        .map_err(map_store)?;
    Ok(Json(json!(updated)))
}

async fn require_known_host(state: &AppState, host_id: &str) -> Result<(), HubError> {
    match state
        .store
        .get_host(host_id.to_string())
        .await
        .map_err(map_store)?
    {
        Some(_) => Ok(()),
        None => Err(HubError::BadRequest(format!(
            "unknown host {host_id}; enroll it and register a workspace first"
        ))),
    }
}

/// Validate per-host capacity quotas: every host must be enrolled.
async fn resolve_hosts(
    state: &AppState,
    hosts: Vec<ProjectHostQuota>,
) -> Result<Vec<ProjectHostQuota>, HubError> {
    for quota in &hosts {
        require_known_host(state, quota.host_id.as_id().as_str()).await?;
        for block in &quota.port_blocks {
            let parts: Vec<&str> = block.split('-').collect();
            if parts.len() != 2
                || parts
                    .iter()
                    .any(|part| part.parse::<i64>().is_err() || part.starts_with('-'))
                || parts[0].parse::<i64>().unwrap_or(-1) > parts[1].parse::<i64>().unwrap_or(-1)
            {
                return Err(HubError::BadRequest(format!(
                    "host {} declares a bad port block {block}; want START-END",
                    quota.host_id.as_id()
                )));
            }
        }
    }
    Ok(hosts)
}

/// Validate member `(hostId, workspaceId)` pairs: the host must be known and
/// the workspace must appear in its last acknowledged D-023 snapshot.
async fn resolve_members(
    state: &AppState,
    bodies: &[MemberBody],
) -> Result<Vec<ProjectMember>, HubError> {
    let mut members = Vec::new();
    for body in bodies {
        let host_id = parse_branded::<remuda_protocol::HostId>(body.host_id.clone())
            .map_err(|err| HubError::BadRequest(format!("member.hostId: {err}")))?;
        let workspace_id = parse_branded::<remuda_protocol::WorkspaceId>(body.workspace_id.clone())
            .map_err(|err| HubError::BadRequest(format!("member.workspaceId: {err}")))?;
        let host = state
            .store
            .get_host(host_id.as_id().to_string())
            .await
            .map_err(map_store)?
            .ok_or_else(|| {
                HubError::BadRequest(format!("member host {} is not enrolled", host_id.as_id()))
            })?;
        if !host.workspaces.iter().any(|workspace| {
            workspace.get("workspaceId").and_then(Value::as_str)
                == Some(workspace_id.as_id().as_str())
        }) {
            return Err(HubError::BadRequest(format!(
                "workspace {} is not registered on host {}",
                workspace_id.as_id(),
                host_id.as_id()
            )));
        }
        members.push(ProjectMember {
            host_id,
            workspace_id,
            role: body.role.clone().unwrap_or_else(|| "member".into()),
        });
    }
    Ok(members)
}

fn parse_branded<T>(raw: String) -> Result<T, String>
where
    T: TryFrom<String>,
    T::Error: std::fmt::Display,
{
    T::try_from(raw).map_err(|err| err.to_string())
}

fn parse_timestamp(now: &str) -> Result<remuda_protocol::Timestamp, HubError> {
    remuda_protocol::Timestamp::try_from(now.to_string())
        .map_err(|err| HubError::Internal(format!("clock produced a bad timestamp: {err}")))
}

// ─── Store-backed CRUD ──────────────────────────────────────────────────────

impl Store {
    /// Insert a fully-built project document.
    pub async fn insert_project(
        &self,
        project: Project,
        created_by: String,
    ) -> Result<Project, StoreError> {
        self.run_named("insert_project", move |conn| {
            let id = project.meta.id.as_id().to_string();
            let now = crate::config::now_rfc3339();
            let doc = serde_json::to_string(&project)?;
            conn.execute(
                "INSERT INTO projects (id, name, doc_json, revision, created_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5)",
                params![id, project.name, doc, created_by, now],
            )?;
            load_project(conn, &id)?.ok_or_else(|| StoreError::Id("project insert missing".into()))
        })
        .await
    }

    /// List projects, oldest first.
    pub async fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        self.run_named("list_projects", |conn| {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY created_at")?;
            let ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_project(conn, &id)?
                        .ok_or_else(|| StoreError::Id("project vanished".into()))
                })
                .collect()
        })
        .await
    }

    /// One project.
    pub async fn get_project(&self, project_id: String) -> Result<Option<Project>, StoreError> {
        self.run_named("get_project", move |conn| load_project(conn, &project_id))
            .await
    }

    /// Apply a mutation to a project doc, bumping revision and updated_at.
    pub async fn patch_project<F>(
        &self,
        project_id: String,
        mutate: F,
    ) -> Result<Option<Project>, StoreError>
    where
        F: FnOnce(&mut Project) -> Result<(), StoreError> + Send + 'static,
    {
        self.run_named("patch_project", move |conn| {
            let Some(mut project) = load_project(conn, &project_id)? else {
                return Ok(None);
            };
            mutate(&mut project)?;
            let now = crate::config::now_rfc3339();
            project.meta.revision = U64(project.meta.revision.0 + 1);
            project.meta.updated_at =
                remuda_protocol::Timestamp::try_from(now.clone())
                    .map_err(|err| StoreError::Id(format!("bad timestamp: {err}")))?;
            let doc = serde_json::to_string(&project)?;
            conn.execute(
                "UPDATE projects SET doc_json = ?1, name = ?2, revision = revision + 1, updated_at = ?3
                 WHERE id = ?4",
                params![doc, project.name, now, project_id],
            )?;
            load_project(conn, &project_id)
        })
        .await
    }

    /// Delete a project.
    pub async fn delete_project(&self, project_id: String) -> Result<Option<Project>, StoreError> {
        self.run_named("delete_project", move |conn| {
            let existing = load_project(conn, &project_id)?;
            conn.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;
            Ok(existing)
        })
        .await
    }
}

fn load_project(conn: &Connection, id: &str) -> Result<Option<Project>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM projects WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(StoreError::from)
}
