//! Worker roster + the `dispatch` / `retire` / `hostcap` server-side verbs;
//! M1 batch 5a (coordinator-hierarchy.md §1.1 goal 6, §2.2④, §2.4, §3.4).
//!
//! Dispatch productises the coordinator's spawn scripts: the Hub assigns name,
//! branch, worktree, target dir and port block, provisions through the Node,
//! admits supply via co-supply, launches through the existing instance-create
//! path, and delivers the brief as an object attachment — never inline.
//! Retire closes the herdr carrier and reclaims worktree + target dir through
//! the Node. Cross-layer state lives in the roster, not in a shell script.

use crate::AppState;
use crate::agent_scope::{caller, caller_project_scope, require_grant};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{HostRecord, InstanceDelegation, Store, StoreError};
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_protocol::{
    EntityMeta, InstanceScope, ProjectId, TaskSpec, U64, WorkerRoster, WorkerRosterId, WorkerState,
    slugify, validate_worker_branch, validate_worker_name,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

/// Default size (ports) of one allocated worker port block.
const PORT_BLOCK_SIZE: i64 = 10;
/// Default allocatable range when a project declares no `portBlocks`. Sits
/// below the gate lanes (58970+) and the hub-e2e fixed ports.
const DEFAULT_PORT_RANGE: (i64, i64) = (58600, 58959);
/// Build-jobs cap exported into every worker's tab env (the shared-host cap
/// the coordinator scripts hand-roll as `CARGO_BUILD_JOBS=8`).
const WORKER_BUILD_JOBS: i64 = 8;

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS worker_roster (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            name TEXT NOT NULL,
            host_id TEXT NOT NULL,
            state_kind TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS worker_roster_project ON worker_roster(project_id);
         CREATE INDEX IF NOT EXISTS worker_roster_host ON worker_roster(host_id);
         CREATE INDEX IF NOT EXISTS worker_roster_active_name
             ON worker_roster(project_id, name) WHERE state_kind != 'retired';",
    )?;
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workers", get(list_workers))
        .route("/v1/workers/dispatch", post(dispatch_worker))
        .route("/v1/workers/{id}", get(get_worker))
        .route("/v1/workers/{id}/retire", post(retire_worker))
        .route("/v1/workers/{id}/state", post(set_worker_state))
        .route("/v1/workers/{id}/brief", post(send_worker_brief))
        .route("/v1/hosts/{id}/hostcap", get(host_capacity))
}

// ── request bodies ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatchBody {
    /// Brief text (utf8). The CLI reads the brief file and posts the bytes;
    /// the Hub stores it as an object attachment, never as inline prompt text.
    pub(crate) brief: String,
    /// Original brief file name (sanitised for the attachment name).
    #[serde(default)]
    pub(crate) brief_name: Option<String>,
    /// Owning project `prj_…`.
    pub(crate) project_id: ProjectId,
    /// Optional bound task.
    #[serde(default)]
    pub(crate) task_id: Option<String>,
    /// `claude` (default) / `codex` / `grok`.
    #[serde(default)]
    pub(crate) harness: Option<String>,
    /// Explicit model id; pins supply admission.
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// Explicit worker name (one safe segment).
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Explicit host (`hst_…`).
    #[serde(default)]
    pub(crate) host_id: Option<String>,
    /// `local` / `remote` tendency; resolved against project member hosts.
    #[serde(default)]
    pub(crate) placement: Option<String>,
    /// Optional explicit driver override. Honoured verbatim or refused with a
    /// reason (409), never silently replaced. The default prefers the Node's
    /// native `shell-pty` carrier when it reports the carrier launchable, then
    /// herdr's `claude-pty`; `claude-print` is never a default (D-035).
    #[serde(default)]
    pub(crate) driver: Option<String>,
    /// Carrier preference batch 6 (D-034): `native` (shell-pty), `herdr`, or
    /// `print`. Default order is native → herdr; print is only ever picked on
    /// this explicit request, never silently.
    #[serde(default)]
    pub(crate) carrier: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListWorkersQuery {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetireBody {
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateBody {
    state: String,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BriefBody {
    content: String,
    #[serde(default)]
    name: Option<String>,
}

// ── list / get ─────────────────────────────────────────────────────────────

async fn list_workers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListWorkersQuery>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let mut items = state
        .store
        .list_workers(query.project.clone())
        .await
        .map_err(map_store)?;
    items.retain(|worker| {
        scope.allows_project(worker.project_id.as_id().as_str())
            && query
                .state
                .as_deref()
                .is_none_or(|wanted| worker.state.kind() == wanted)
    });
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

async fn get_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    Ok(Json(json!(worker)))
}

/// Resolve a roster row by `wkr_…` id or active worker name within the
/// caller's project scope.
pub(crate) async fn resolve_worker(
    state: &AppState,
    id_or_name: &str,
    scope: &InstanceScope,
) -> Result<WorkerRoster, HubError> {
    let worker = if id_or_name.starts_with("wkr_") {
        state
            .store
            .get_worker(id_or_name.to_string())
            .await
            .map_err(map_store)?
            .ok_or(HubError::NotFound)?
    } else {
        let mut matches = state
            .store
            .find_active_workers_named(id_or_name.to_string())
            .await
            .map_err(map_store)?;
        matches.retain(|worker| scope.allows_project(worker.project_id.as_id().as_str()));
        match matches.len() {
            0 => return Err(HubError::NotFound),
            1 => matches.remove(0),
            _ => {
                return Err(HubError::Conflict(format!(
                    "worker name {id_or_name} matches multiple projects; use its wkr_ id"
                )));
            }
        }
    };
    if !scope.allows_project(worker.project_id.as_id().as_str()) {
        return Err(HubError::Forbidden);
    }
    Ok(worker)
}

// ── dispatch ───────────────────────────────────────────────────────────────

async fn dispatch_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DispatchBody>,
) -> Result<Json<Value>, HubError> {
    dispatch_core(State(state), headers, body).await
}

/// Dispatch a worker from a validated body. Shared by the HTTP route and
/// `worker replace` (5b), which retires a dead worker then re-dispatches the
/// same brief/name through this exact path.
pub(crate) async fn dispatch_core(
    state: State<AppState>,
    headers: HeaderMap,
    body: DispatchBody,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(body.project_id.as_id().as_str()) {
        return Err(HubError::Forbidden);
    }
    if body.brief.trim().is_empty() {
        return Err(HubError::BadRequest("brief is required".into()));
    }
    let harness = match body.harness.as_deref().unwrap_or("claude") {
        "claude" | "codex" | "grok" => body.harness.as_deref().unwrap_or("claude").to_string(),
        other => {
            return Err(HubError::BadRequest(format!(
                "harness must be claude, codex or grok (got {other})"
            )));
        }
    };

    let project = state
        .store
        .get_project(body.project_id.as_id().to_string())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;

    // ── host selection (§3.4) ──────────────────────────────────────────────
    let (host, placement_warnings) = select_dispatch_host(
        &state,
        &project,
        body.host_id.as_deref(),
        body.placement.as_deref(),
    )
    .await?;
    let workspace_id = project
        .members
        .iter()
        .find(|member| member.host_id.as_id().as_str() == host.host_id)
        .map(|member| member.workspace_id.clone())
        .ok_or_else(|| {
            HubError::BadRequest(format!(
                "project has no registered workspace on host {}; add a member first",
                host.host_id
            ))
        })?;

    // ── supply admission (co-supply §4.4) ──────────────────────────────────
    let task = match body.task_id.as_deref() {
        Some(id) => Some(
            state
                .store
                .get_task(id.to_string())
                .await
                .map_err(map_store)?
                .ok_or(HubError::NotFound)?,
        ),
        None => None,
    };
    let mut model = body.model.clone();
    let mut provider_profile_id: Option<String> = None;
    let mut delegation: Option<String> = None;
    let mut supply_decision: Option<Value> = None;
    let mut admission_warnings: Vec<String> = Vec::new();
    let profiles = state.store.list_providers(None).await.map_err(map_store)?;
    let supply_declared = !profiles.is_empty() || project.provider.profile_id.is_some();
    if supply_declared {
        let task_spec = build_task_spec(&project, task.as_ref(), &harness, body.model.as_deref());
        let decision =
            crate::supply::solve_state(&state, &task_spec, std::slice::from_ref(&host), true)
                .await?;
        // A pin matched nothing: hard 409 before any name/port allocation or
        // Node provisioning (§4.3 — refuse, never substitute).
        if let Some(refusal) = &decision.pin_refusal {
            return Err(crate::supply::pin_refused_error(refusal));
        }
        if decision.deferred {
            return Err(HubError::SupplyDeferred {
                decision: decision.to_json(),
            });
        }
        // A pinned (explicit --model) choice bypasses the rank filters, so the
        // parked-model refusal is enforced separately: never launch a model
        // whose observed window is cooling (the 429 park reason).
        if let Some(wanted) = &model
            && let Some(park) = crate::supply::model_park_reason(&profiles, wanted, Value::Null)
        {
            return Err(HubError::SupplyDeferred {
                decision: json!({
                    "deferred": true,
                    "reasons": [park],
                    "chosen": null,
                    "ranked": [],
                    "rejected": [],
                }),
            });
        }
        let chosen = decision
            .chosen
            .as_ref()
            .expect("non-deferred, non-refused supply decision has a choice");
        // Informational only: an honored pin that diverges from the project's
        // declared workhorse (e.g. an explicit frontier on a workhorse task).
        if let Some(warning) = crate::supply::pin_workhorse_warning(
            &chosen.model_id,
            project.model_roles.workhorse.as_deref(),
            body.model.is_some(),
        ) {
            admission_warnings.push(warning);
        }
        model = Some(chosen.model_id.clone());
        provider_profile_id = Some(chosen.profile_id.clone());
        let profile = state
            .store
            .get_provider(chosen.profile_id.clone())
            .await
            .map_err(map_store)?
            .ok_or_else(|| {
                HubError::BadRequest("admission named a profile that vanished".into())
            })?;
        delegation = Some(match profile.kind.as_str() {
            "native" => "none".to_string(),
            "direct" => "direct".to_string(),
            _ => "gateway".to_string(),
        });
        supply_decision = Some(decision.to_json());
    } else if model.is_none() {
        model = project.model_roles.workhorse.clone();
    }

    // ── product-assigned name / branch / port block (§2.4) ─────────────────
    let slug_source = task
        .as_ref()
        .map(|task| task.title.clone())
        .or_else(|| body.brief_name.clone())
        .unwrap_or_else(|| "work".into());
    let name = match body.name.as_deref() {
        Some(name) => {
            validate_worker_name(name).map_err(HubError::BadRequest)?;
            name.to_string()
        }
        None => allocate_worker_name(&state, &project, &slug_source).await?,
    };
    let slug = unique_slug(&state, &project, &name, &slugify(&slug_source)).await?;
    let branch = format!("wt/{name}/{slug}");
    validate_worker_branch(&branch).map_err(HubError::BadRequest)?;
    if let Some(existing) = state
        .store
        .find_active_workers_named(name.clone())
        .await
        .map_err(map_store)?
        .into_iter()
        .find(|worker| worker.project_id == project.meta.id)
    {
        return Err(HubError::Conflict(format!(
            "worker {name} is already active in this project as {}",
            existing.meta.id.as_id()
        )));
    }
    let port_block = allocate_port_block(&state, &project, &host.host_id).await?;

    // ── carrier choice, before anything is provisioned ─────────────────────
    // An explicit driver/carrier is honoured verbatim or refused; the default
    // comes from what this host reports it can actually launch. Deciding here
    // means a refusal costs no worktree, so there is nothing to roll back.
    let driver = match body.driver.as_deref() {
        Some(explicit) => select_worker_driver(&harness, explicit, &host)?,
        None => select_carrier(&harness, &host, body.carrier.as_deref())?,
    };

    // ── provision through the Node (worktree + target dir) ─────────────────
    let provision = crate::http::call_node(
        &state,
        &host.host_id,
        "worker.provision",
        json!({
            "name": name,
            "branch": branch,
            "workspaceId": workspace_id.as_id(),
            "startPoint": format!("origin/{}", project.default_base_branch),
        }),
    )
    .await?;
    let worktree_path = provision
        .get("worktreePath")
        .and_then(Value::as_str)
        .ok_or_else(|| HubError::Internal("worker.provision returned no worktreePath".into()))?
        .to_string();
    let target_dir = provision
        .get("targetDir")
        .and_then(Value::as_str)
        .map(str::to_string);

    // ── launch env: per-worker target dir, build cap, port block ───────────
    let extra_env = worker_extra_env(target_dir.as_deref(), port_block.as_deref());
    let mut spec = worker_launch_spec(
        &harness,
        &driver,
        &model,
        &provider_profile_id,
        &delegation,
        workspace_id.as_id().as_str(),
        &host.host_id,
        &worktree_path,
        &name,
        project.meta.id.as_id().as_str(),
        task.as_ref().map(|task| task.meta.id.as_id().as_str()),
        extra_env,
    );
    crate::agent_scope::prepare_create(
        &state,
        &headers,
        &device,
        &host.host_id,
        &driver,
        &mut spec,
    )
    .await?;
    // Fold host launch defaults (binary path); dispatch already set tui/args.
    if let Some(obj) = spec.as_object_mut()
        && let Some(path) = host
            .claude_binary_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        && obj.get("binaryPath").is_none_or(Value::is_null)
    {
        obj.insert("binaryPath".into(), json!(path));
    }
    // The project provider layer folds into the waterfall like it does on the
    // normal create path (explicit > project > host > global).
    crate::providers::resolve_and_attach_with_project(
        &state,
        &host,
        &mut spec,
        Some(&project.provider),
    )
    .await?;

    let delegation_tree = InstanceDelegation {
        role: Some("worker".into()),
        scope: InstanceScope {
            project_ids: vec![project.meta.id.clone()],
            ..Default::default()
        },
        grants: Vec::new(),
        task_id: task.as_ref().map(|task| task.meta.id.as_id().to_string()),
        enforce_tree: true,
    };
    // Capture what a rollback needs before the block below moves these.
    let rollback_name = name.clone();
    let rollback_workspace = workspace_id.clone();
    let requested_driver = driver.clone();

    // Everything from here on runs with a worktree and target dir already on
    // the Node. No roster row exists yet, so `remuda retire` cannot reclaim
    // them — a failure that merely returned would leak both and force manual
    // cleanup (observed 2026-09-17; D-035). One unwind point covers it.
    let dispatched = async {
        let (instance, command) = crate::placement::spawn_on_host(
            &state,
            &host,
            crate::placement::SpawnRequest {
                kind: harness.clone(),
                driver,
                workspace_id: Some(workspace_id.as_id().to_string()),
                title: Some(name.clone()),
                prompt: None,
                spec: spec.clone(),
                operation: "instance.create",
                idempotency_key: None,
                delegation: delegation_tree,
            },
        )
        .await?;

        // An explicit --host pin over its CPU/mem ceiling was admitted, not
        // refused: journal the saturation so the worker session shows why.
        for warning in &placement_warnings {
            crate::ws::publish_hub_diagnostic(
                &state,
                &instance.instance_id,
                "placement_resource_warning",
                warning,
            )
            .await;
        }

        // ── deliver the brief as an object attachment, then prompt ────────────
        let brief_name = body
            .brief_name
            .as_deref()
            .map(sanitize_brief_name)
            .transpose()
            .map_err(|bad| HubError::BadRequest(format!("invalid brief name {bad}")))?
            .unwrap_or_else(|| "brief.md".into());
        let (object_id, send_payload) = stage_brief(
            &state,
            &instance.instance_id,
            &host.host_id,
            &device.id,
            &body.brief,
            &brief_name,
        )
        .await?;
        let (send_command, _) = state
            .store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host.host_id.clone(),
                "instance.send".into(),
                send_payload,
                None,
            )
            .await
            .map_err(map_store)?;
        let live = state.nodes.kind_of(&host.host_id).await.is_some();
        let _send = crate::http::forward_if_online(&state, send_command, live).await?;

        // What the Node actually built. `spawn_on_host` forwarded the create and
        // `forward_if_online` reconciled the instance row from the Node's reply, so
        // re-reading the row is what tells us whether the request was honoured.
        let ran_driver = state
            .store
            .get_instance(instance.instance_id.clone())
            .await
            .map_err(map_store)?
            .map(|record| record.driver)
            .filter(|driver| !driver.is_empty())
            .unwrap_or_else(|| requested_driver.clone());
        if ran_driver != requested_driver {
            crate::ws::publish_hub_diagnostic(
                &state,
                &instance.instance_id,
                "driver_downgraded",
                &format!("requested driver {requested_driver} but the node built {ran_driver}"),
            )
            .await;
        }

        // ── roster row ─────────────────────────────────────────────────────────
        let now = crate::config::now_rfc3339();
        let worker = WorkerRoster {
            meta: EntityMeta {
                id: WorkerRosterId::new(),
                revision: U64(1),
                created_at: remuda_protocol::Timestamp::try_from(now.clone())
                    .map_err(|err| HubError::Internal(err.to_string()))?,
                updated_at: remuda_protocol::Timestamp::try_from(now)
                    .map_err(|err| HubError::Internal(err.to_string()))?,
            },
            project_id: project.meta.id.clone(),
            name,
            instance_id: Some(instance.instance_id.parse().map_err(
                |err: remuda_protocol::WireValueError| HubError::BadRequest(err.to_string()),
            )?),
            host_id: host
                .host_id
                .parse()
                .map_err(|err: remuda_protocol::WireValueError| {
                    HubError::BadRequest(err.to_string())
                })?,
            workspace_id,
            harness,
            // The driver the Node reports it actually built, read back from the
            // instance row `forward_if_online` reconciled from the create reply —
            // not the one the Hub asked for. These diverged silently before
            // (D-035; docs/design/evidence/dispatch-driver-1.md).
            driver: Some(ran_driver),
            model,
            provider_profile_id,
            branch,
            worktree_path,
            port_block,
            target_dir,
            brief_object_id: Some(object_id),
            task_id: task.as_ref().map(|task| task.meta.id.clone()),
            state: WorkerState::Working,
            watch: None,
            last_nudge_at: None,
            resumed_from: None,
            replace_count: None,
            supply_decision,
            reclaimed_bytes: None,
        };
        let row = state
            .store
            .insert_worker(worker, device.id.clone())
            .await
            .map_err(map_store)?;
        let _ = command;
        state
            .store
            .append_audit(
                device.id,
                "worker.dispatch".into(),
                Some(row.meta.id.as_id().to_string()),
                json!({ "project": row.project_id.as_id(), "host": row.host_id.as_id() }),
            )
            .await
            .map_err(map_store)?;
        Ok::<_, HubError>((row, instance))
    }
    .await;
    let (row, instance) = match dispatched {
        Ok(value) => value,
        Err(error) => {
            rollback_provisioned(
                &state,
                &host.host_id,
                &rollback_name,
                &rollback_workspace,
                &error,
            )
            .await;
            return Err(error);
        }
    };
    Ok(Json(json!({
        "worker": row,
        "instanceId": instance.instance_id,
        "warnings": placement_warnings.into_iter().chain(admission_warnings).collect::<Vec<_>>(),
    })))
}

/// Give back the worktree and target dir `worker.provision` created, when a
/// later dispatch step failed before any roster row existed.
///
/// Best effort by construction: the caller is already returning the original
/// error, and a reclaim that also fails must not replace it with a less useful
/// one. A failure here is logged so the leak is at least visible, never silent.
async fn rollback_provisioned(
    state: &AppState,
    host_id: &str,
    name: &str,
    workspace_id: &remuda_protocol::WorkspaceId,
    cause: &HubError,
) {
    match crate::http::call_node(
        state,
        host_id,
        "worker.remove",
        json!({ "name": name, "workspaceId": workspace_id.as_id() }),
    )
    .await
    {
        Ok(_) => tracing::warn!(
            %host_id, %name, error = %cause,
            "dispatch failed after provisioning; worktree and target dir reclaimed"
        ),
        Err(error) => tracing::error!(
            %host_id, %name, rollback_error = %error, error = %cause,
            "dispatch failed after provisioning and the reclaim failed too; \
             worktree and target dir are leaked on the node"
        ),
    }
}

/// Select the dispatch host per §3.4: explicit host, else project members
/// filtered/ordered by the local/remote tendency, then least-loaded.
///
/// Returns the chosen host plus any pin warnings: an explicit `--host` over
/// its CPU/mem ceiling is an operator instruction, so it is admitted with a
/// warning (and the warning is journaled against the spawned instance),
/// never refused.
async fn select_dispatch_host(
    state: &AppState,
    project: &remuda_protocol::Project,
    explicit: Option<&str>,
    tendency: Option<&str>,
) -> Result<(HostRecord, Vec<String>), HubError> {
    let placement = if let Some(host) = explicit {
        crate::placement::Placement::Host {
            host_id: host.to_string(),
        }
    } else {
        crate::placement::Placement::Project {
            project_id: project.meta.id.as_id().to_string(),
        }
    };
    // Placement must not pre-judge the carrier: `claude-pty` here made
    // `pick_hosts` reject every host without herdr, including a Node whose
    // native `shell-pty` carrier can launch perfectly well. `shell-pty` carries
    // no herdr requirement, so the driver choice stays with `driver_for` /
    // `select_worker_driver`, which read what the host actually reports.
    let spec = crate::placement::PlaceSpec {
        driver: "shell-pty".into(),
        delegation: None,
    };
    let outcome = crate::placement::pick_hosts(state, &placement, &spec).await?;
    let mut hosts = outcome.hosts;
    let warnings = outcome.warnings;
    if explicit.is_none()
        && let Some(wanted) = tendency
    {
        let home = project.home_host.as_ref();
        let classified = hosts
            .into_iter()
            .filter(|host| matches_tendency(project, host, wanted, home))
            .collect::<Vec<_>>();
        if classified.is_empty() {
            return Err(HubError::Unsatisfiable {
                reasons: vec![format!(
                    "no {wanted} member host satisfies capacity; retry on the other placement or --host"
                )],
            });
        }
        hosts = classified;
    }
    hosts
        .into_iter()
        .next()
        .map(|host| (host, warnings))
        .ok_or_else(|| HubError::Unsatisfiable {
            reasons: vec!["placement returned no host".into()],
        })
}

fn matches_tendency(
    project: &remuda_protocol::Project,
    host: &HostRecord,
    tendency: &str,
    home: Option<&remuda_protocol::HostId>,
) -> bool {
    let latency = project
        .hosts
        .iter()
        .find(|quota| quota.host_id.as_id().as_str() == host.host_id)
        .and_then(|quota| quota.latency_class.as_deref());
    match tendency {
        "local" => {
            latency == Some("local") || home.is_some_and(|id| id.as_id().as_str() == host.host_id)
        }
        "remote" => latency == Some("remote"),
        _ => true,
    }
}

/// Default carrier choice (used by resume, which cannot prompt): the native
/// shell-pty the Node advertises as launchable, then herdr.
///
/// There is deliberately **no** print fallback. `claude-print` ends its session
/// after one turn and then needs a manual resume, so a worker on it can never be
/// nudged, steered or watched: a host that carries neither interactive carrier is
/// refused with the reason instead of being downgraded into a product nobody
/// asked for. That silent downgrade is exactly what once put a dispatch on
/// `claude-print` while the roster said `claude-pty`
/// (`docs/design/evidence/dispatch-driver-1.md`, D-035).
pub(crate) fn driver_for(harness: &str, host: &HostRecord) -> Result<String, HubError> {
    select_carrier(harness, host, None)
}

/// Whether the Node advertised `shell-pty` as launchable in its hello
/// `capabilities.driverInventory` (D-028 §5.1).
pub(crate) fn native_carrier_works(host: &HostRecord) -> bool {
    host.capabilities
        .get("driverInventory")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("kind").and_then(Value::as_str) == Some("shell-pty"))
        .any(|item| item.get("launchable").and_then(Value::as_bool) == Some(true))
}

/// Batch 6 carrier preference (`remuda dispatch --carrier`):
/// default native when the Node reports the native carrier works, else herdr
/// when advertised; `print` is never selected unless explicitly requested.
pub(crate) fn select_carrier(
    harness: &str,
    host: &HostRecord,
    explicit: Option<&str>,
) -> Result<String, HubError> {
    if harness != "claude" {
        // codex/grok only have the herdr-backed generic pty driver.
        return Ok("generic-pty".to_string());
    }
    let has_herdr = host.herdr.as_ref().is_some_and(|value| !value.is_null());
    let native = || native_carrier_works(host);
    match explicit {
        Some("native") | Some("shell-pty") => {
            if native() {
                Ok("shell-pty".to_string())
            } else {
                Err(HubError::Unsatisfiable {
                    reasons: vec![
                        "host Node does not advertise a launchable native shell-pty carrier".into(),
                    ],
                })
            }
        }
        Some("herdr") | Some("claude-pty") => {
            if has_herdr {
                Ok("claude-pty".to_string())
            } else {
                Err(HubError::Unsatisfiable {
                    reasons: vec!["host does not advertise herdr".into()],
                })
            }
        }
        Some("print") | Some("claude-print") => Ok("claude-print".to_string()),
        Some(other) => Err(HubError::BadRequest(format!(
            "carrier must be native|herdr|print, got {other:?}"
        ))),
        None => {
            if native() {
                Ok("shell-pty".to_string())
            } else if has_herdr {
                Ok("claude-pty".to_string())
            } else {
                Err(HubError::Unsatisfiable {
                    reasons: vec![format!(
                        "host {} can carry no interactive {harness} driver: it reports no launchable shell-pty (set REMUDA_PTY_CARRIER=native on the Node) and advertises no herdr; claude-print is never a default because it ends the session after one turn (D-035)",
                        host.host_id
                    )],
                })
            }
        }
    }
}

/// Validate an explicit `dispatch --driver` override. Only Claude may pick the
/// native `shell-pty` / `claude-print` / `claude-pty` carriers; codex and grok
/// always run on `generic-pty`.
fn select_worker_driver(
    harness: &str,
    explicit: &str,
    host: &HostRecord,
) -> Result<String, HubError> {
    match (harness, explicit) {
        ("claude", driver @ ("claude-pty" | "shell-pty" | "claude-print")) => {
            // An operator instruction that this host cannot honour is refused
            // with the reason, never quietly swapped for a carrier that works:
            // that swap is how a dispatch recorded one product and ran another.
            if driver == "shell-pty" && !native_carrier_works(host) {
                return Err(HubError::Conflict(format!(
                    "host {} does not report shell-pty as launchable; set \
                     REMUDA_PTY_CARRIER=native on the Node or dispatch without --driver",
                    host.host_id
                )));
            }
            // `claude-pty` is the only allowlisted driver that needs herdr;
            // `claude-bg` never reaches here (it is not in the match arm above).
            if driver == "claude-pty" && host.herdr.as_ref().is_none_or(Value::is_null) {
                return Err(HubError::Conflict(format!(
                    "host {} advertises no herdr, which {driver} requires; dispatch without \
                     --driver to use the native carrier",
                    host.host_id
                )));
            }
            Ok(driver.to_string())
        }
        ("codex" | "grok", "generic-pty") => Ok("generic-pty".to_string()),
        _ => Err(HubError::BadRequest(format!(
            "driver {explicit} is not valid for harness {harness}"
        ))),
    }
}

/// Product-assigned tab env for one worker launch: per-worker cargo target
/// dir, the shared-host build cap, and the worker's e2e port pair. Shared by
/// dispatch and `worker resume` (5b) so a respawned worker keeps the same env.
pub(crate) fn worker_extra_env(
    target_dir: Option<&str>,
    port_block: Option<&str>,
) -> serde_json::Map<String, Value> {
    let mut extra_env = serde_json::Map::new();
    if let Some(target) = target_dir {
        extra_env.insert("CARGO_TARGET_DIR".into(), json!(target));
    }
    extra_env.insert("CARGO_INCREMENTAL".into(), json!("0"));
    extra_env.insert("CARGO_BUILD_JOBS".into(), json!(WORKER_BUILD_JOBS));
    if let Some(block) = port_block
        && let Some((first, _last)) = parse_block(block)
    {
        extra_env.insert("HUB_E2E_LISTEN".into(), json!(first.to_string()));
        extra_env.insert(
            "HUB_E2E_WEB_PORT".into(),
            json!((first + PORT_BLOCK_SIZE - 1).to_string()),
        );
    }
    extra_env
}

/// Base instance-create spec for one worker, identical for the first launch
/// and for a same-worktree respawn (`worker resume`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn worker_launch_spec(
    harness: &str,
    driver: &str,
    model: &Option<String>,
    provider_profile_id: &Option<String>,
    delegation: &Option<String>,
    workspace_id: &str,
    host_id: &str,
    cwd: &str,
    name: &str,
    project_id: &str,
    task_id: Option<&str>,
    extra_env: serde_json::Map<String, Value>,
) -> Value {
    let mut spec = json!({
        "kind": harness,
        "driver": driver,
        "model": model,
        "providerProfileId": provider_profile_id,
        "delegation": delegation,
        "permissionMode": "bypassPermissions",
        "workspaceId": workspace_id,
        "hostId": host_id,
        "cwd": cwd,
        "name": name,
        "title": name,
        "prompt": null,
        "extraEnv": Value::Object(extra_env),
        "projectId": project_id,
    });
    if let Some(task_id) = task_id {
        spec["taskId"] = json!(task_id);
    }
    spec
}

fn build_task_spec(
    project: &remuda_protocol::Project,
    task: Option<&remuda_protocol::Task>,
    harness: &str,
    model: Option<&str>,
) -> TaskSpec {
    let class = task.map(|task| task.class).unwrap_or_default();
    TaskSpec {
        task_id: task.map(|task| task.meta.id.clone()),
        project_id: Some(project.meta.id.clone()),
        parent: task.and_then(|task| task.parent_task_id.clone()),
        class,
        effort: project.default_effort.clone(),
        pin: model.map(|wanted| remuda_protocol::TaskPin {
            harness: Some(harness.to_string()),
            model: Some(wanted.to_string()),
            supply_id: None,
        }),
        requires: project
            .hosts
            .iter()
            .flat_map(|quota| quota.requires.clone())
            .collect(),
        ..Default::default()
    }
}

/// Allocate a worker name when the caller did not pass one: `w<4-hex>` from a
/// fresh id, guaranteed to collide only astronomically / verified below.
async fn allocate_worker_name(
    state: &AppState,
    project: &remuda_protocol::Project,
    _slug_source: &str,
) -> Result<String, HubError> {
    for _ in 0..10 {
        let candidate = format!("w{}", &WorkerRosterId::new().as_id().to_string()[4..8]);
        validate_worker_name(&candidate).map_err(HubError::BadRequest)?;
        let taken = state
            .store
            .find_active_workers_named(candidate.clone())
            .await
            .map_err(map_store)?
            .iter()
            .any(|worker| worker.project_id == project.meta.id);
        if !taken {
            return Ok(candidate);
        }
    }
    Err(HubError::Internal(
        "could not allocate a unique worker name".into(),
    ))
}

/// Append `-2`, `-3`, … to the slug until the branch is free in the project.
async fn unique_slug(
    state: &AppState,
    project: &remuda_protocol::Project,
    name: &str,
    base: &str,
) -> Result<String, HubError> {
    let active = state
        .store
        .list_workers(Some(project.meta.id.as_id().to_string()))
        .await
        .map_err(map_store)?;
    let slug = slugify(base);
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            slug.clone()
        } else {
            let mut candidate = format!("{slug}-{n}");
            candidate.truncate(48);
            candidate
        };
        let branch = format!("wt/{name}/{candidate}");
        // Retired rows keep their branch (gate/land owns branch deletion), so
        // a `worker replace` reusing the name must take a fresh slug rather
        // than collide with the kept branch.
        if !active.iter().any(|worker| worker.branch == branch) {
            return Ok(candidate);
        }
        n += 1;
        if n > 1000 {
            return Err(HubError::Internal("could not allocate unique slug".into()));
        }
    }
}

/// Allocate the next free port block for this host from the project's
/// declared ranges (or the built-in default), scanning active roster rows so
/// two workers never share a block.
async fn allocate_port_block(
    state: &AppState,
    project: &remuda_protocol::Project,
    host_id: &str,
) -> Result<Option<String>, HubError> {
    let ranges: Vec<(i64, i64)> = project
        .hosts
        .iter()
        .filter(|quota| quota.host_id.as_id().as_str() == host_id)
        .flat_map(|quota| quota.port_blocks.iter())
        .filter_map(|block| parse_block(block))
        .collect();
    let project_has_host = project
        .hosts
        .iter()
        .any(|quota| quota.host_id.as_id().as_str() == host_id);
    let ranges = if ranges.is_empty() {
        // A declared project host with no portBlocks simply gets no allocated
        // block (only e2e test workers need one). The built-in range is a
        // fallback for projects that never declared a hosts entry.
        if project_has_host {
            return Ok(None);
        }
        vec![DEFAULT_PORT_RANGE]
    } else {
        ranges
    };
    let active = state
        .store
        .list_workers_for_host(host_id.to_string())
        .await
        .map_err(map_store)?;
    let used: Vec<(i64, i64)> = active
        .iter()
        .filter(|worker| worker.state.is_active())
        .filter_map(|worker| worker.port_block.as_deref())
        .filter_map(parse_block)
        .collect();
    for (start, end) in ranges {
        let mut cursor = start;
        while cursor + PORT_BLOCK_SIZE - 1 <= end {
            let candidate = (cursor, cursor + PORT_BLOCK_SIZE - 1);
            let overlap = used
                .iter()
                .any(|used| cursor <= used.1 && used.0 <= candidate.1);
            if !overlap {
                return Ok(Some(format!("{}-{}", candidate.0, candidate.1)));
            }
            cursor += PORT_BLOCK_SIZE;
        }
    }
    Err(HubError::Conflict(
        "no free port block in this project's ranges; retire a worker first".into(),
    ))
}

fn parse_block(block: &str) -> Option<(i64, i64)> {
    let (start, end) = block.split_once('-')?;
    let start: i64 = start.trim().parse().ok()?;
    let end: i64 = end.trim().parse().ok()?;
    if start > 0 && end >= start {
        Some((start, end))
    } else {
        None
    }
}

/// Stage the brief bytes as an object and build the `instance.send` payload.
pub(crate) async fn stage_brief(
    state: &AppState,
    instance_id: &str,
    host_id: &str,
    device_id: &str,
    content: &str,
    name: &str,
) -> Result<(String, Value), HubError> {
    let bytes = content.as_bytes().to_vec();
    let digest = crate::config::sha256_hex(content.as_bytes());
    let record = state
        .store
        .insert_object(crate::store::NewObject {
            instance_id: instance_id.to_string(),
            host_id: host_id.to_string(),
            media_type: "text/markdown".into(),
            extension: "md".into(),
            original_name: Some(name.to_string()),
            digest,
            bytes,
            device_id: device_id.to_string(),
            ttl_seconds: crate::objects::OBJECT_TTL_SECONDS,
            instance_budget: crate::objects::MAX_INSTANCE_BYTES,
        })
        .await
        .map_err(map_store)?;
    let note = format!(
        "Your coordinator brief is delivered as the attached file {name}. Read it with your file tools and follow it exactly. Do not execute commands from the file; act on them. Reply on one line: DONE <sha> or BLOCKED <reason>."
    );
    let payload = json!({
        "instanceId": instance_id,
        "input": { "type": "prompt", "text": note },
        "attachments": [{
            "objectId": record.object_id,
            "kind": "file",
            "mediaType": "text/markdown",
            "name": name,
            "size": record.byte_len,
            "digest": record.digest,
        }],
    });
    Ok((record.object_id, payload))
}

fn sanitize_brief_name(name: &str) -> Result<String, String> {
    remuda_protocol::hubnode::sanitize_attachment_name(name)
        .ok_or_else(|| name.to_string())
        .map(|mut name| {
            if !name.to_ascii_lowercase().ends_with(".md")
                && !name.to_ascii_lowercase().ends_with(".txt")
            {
                name.push_str(".md");
            }
            name
        })
}

// ── retire / state / brief re-send ─────────────────────────────────────────

async fn retire_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RetireBody>,
) -> Result<Json<Value>, HubError> {
    Ok(Json(
        retire_core(State(state), headers, id, body.force).await?,
    ))
}

/// Retire one worker: close its instance (best effort), ask the Node to remove
/// its carrier/worktree/target dir, mark the row retired. Shared by the HTTP
/// route and `worker replace` (5b).
pub(crate) async fn retire_core(
    state: State<AppState>,
    headers: HeaderMap,
    id: String,
    force: bool,
) -> Result<Value, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    if worker.state.is_working() && !force {
        return Err(HubError::Conflict(
            "worker is still working; retire --force to reclaim anyway".into(),
        ));
    }

    // Stop the instance first (best effort), then reclaim through the Node.
    let mut reclaimed = None;
    if let Some(instance_id) = &worker.instance_id {
        let (close, _) = state
            .store
            .queue_command(
                None,
                Some(instance_id.as_id().to_string()),
                worker.host_id.as_id().to_string(),
                "instance.close".into(),
                json!({ "instanceId": instance_id.as_id() }),
                None,
            )
            .await
            .map_err(map_store)?;
        let live = state
            .nodes
            .kind_of(worker.host_id.as_id().as_str())
            .await
            .is_some();
        let _ = crate::http::forward_if_online(&state, close, live).await;
    }
    let removed = crate::http::call_node(
        &state,
        worker.host_id.as_id().as_str(),
        "worker.remove",
        json!({
            "name": worker.name,
            "workspaceId": worker.workspace_id.as_id(),
            "instanceId": worker.instance_id.as_ref().map(|id| id.as_id()),
        }),
    )
    .await?;
    if let Some(bytes) = removed.get("reclaimedBytes").and_then(Value::as_u64) {
        reclaimed = Some(U64(bytes));
    }

    let updated = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), move |row| {
            row.state = WorkerState::Retired;
            row.reclaimed_bytes = reclaimed;
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    state
        .store
        .append_audit(
            device.id,
            "worker.retire".into(),
            Some(updated.meta.id.as_id().to_string()),
            json!({ "force": force, "reclaimedBytes": reclaimed }),
        )
        .await
        .map_err(map_store)?;
    Ok(json!({ "worker": updated, "node": removed }))
}

async fn set_worker_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<StateBody>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    let next = WorkerState::from_update(&body.state, body.sha.as_deref(), body.reason.as_deref())
        .map_err(HubError::BadRequest)?;
    // A retired worker stays retired (retire is terminal).
    if matches!(worker.state, WorkerState::Retired) {
        return Err(HubError::Conflict("worker is retired".into()));
    }
    let updated = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), move |row| {
            row.state = next;
            Ok(())
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    Ok(Json(json!(updated)))
}

async fn send_worker_brief(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<BriefBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let worker = resolve_worker(&state, &id, &scope).await?;
    if !worker.state.is_active() {
        return Err(HubError::Conflict("worker is retired".into()));
    }
    let instance_id = worker
        .instance_id
        .as_ref()
        .ok_or_else(|| HubError::Conflict("worker never launched an instance".into()))?;
    let name = body
        .name
        .as_deref()
        .map(sanitize_brief_name)
        .transpose()
        .map_err(|bad| HubError::BadRequest(format!("invalid brief name {bad}")))?
        .unwrap_or_else(|| "brief.md".into());
    let (object_id, payload) = stage_brief(
        &state,
        instance_id.as_id().as_str(),
        worker.host_id.as_id().as_str(),
        &device.id,
        &body.content,
        &name,
    )
    .await?;
    let (command, _) = state
        .store
        .queue_command(
            None,
            Some(instance_id.as_id().to_string()),
            worker.host_id.as_id().to_string(),
            "instance.send".into(),
            payload,
            None,
        )
        .await
        .map_err(map_store)?;
    let live = state
        .nodes
        .kind_of(worker.host_id.as_id().as_str())
        .await
        .is_some();
    let command = crate::http::forward_if_online(&state, command, live).await?;
    let updated = state
        .store
        .mutate_worker(worker.meta.id.as_id().to_string(), {
            let object_id = object_id.clone();
            move |row| {
                row.brief_object_id = Some(object_id);
                Ok(())
            }
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    Ok(Json(json!({ "worker": updated, "command": command })))
}

// ── hostcap ────────────────────────────────────────────────────────────────

async fn host_capacity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    require_grant(&state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_host(&host_id) && !scope.is_universe() {
        return Err(HubError::Forbidden);
    }
    let stored = state
        .store
        .get_host(host_id.clone())
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    let online = state.nodes.kind_of(&host_id).await.is_some();
    let host = Store::with_live_link(stored, online);
    let running = state
        .store
        .running_count(host_id.clone())
        .await
        .map_err(map_store)?;
    let active = state
        .store
        .list_workers_for_host(host_id.clone())
        .await
        .map_err(map_store)?
        .into_iter()
        .filter(|worker| worker.state.is_active())
        .collect::<Vec<_>>();
    let resources = host.resources.clone().unwrap_or(json!({}));
    let sampled_at = resources
        .get("sampledAt")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Seconds since the Hub stamped the persisted sample, so a caller can tell
    // a live reading from a fossil without doing its own Rfc3339 clock math.
    let sample_age_sec = crate::placement::resource_sample_age(Some(&resources))
        .map(|age| i64::try_from(age.as_secs()).unwrap_or(i64::MAX));
    let max = host.max_instances;
    let port_blocks = active
        .iter()
        .filter_map(|worker| {
            worker.port_block.as_ref().map(|block| {
                json!({
                    "block": block,
                    "worker": worker.name,
                    "state": worker.state.kind(),
                })
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "hostId": host_id,
        "online": online,
        "cores": resources.get("cpuCount").cloned().unwrap_or(json!(null)),
        "loadPct": resources.get("cpuPct").cloned().unwrap_or(json!(null)),
        "loadAvg1": resources.get("loadAvg1").cloned().unwrap_or(json!(null)),
        "memPct": resources.get("memPct").cloned().unwrap_or(json!(null)),
        "diskFreeGb": resources.get("diskFreeGb").cloned().unwrap_or(json!(null)),
        "sampledAt": sampled_at.map(Value::from).unwrap_or(json!(null)),
        "sampleAgeSec": sample_age_sec.map(Value::from).unwrap_or(json!(null)),
        "maxInstances": max,
        "running": running,
        "freeSlots": (max - running).max(0),
        "activeWorkers": active.len(),
        "portBlocksInUse": port_blocks,
    })))
}

// ── Store CRUD ─────────────────────────────────────────────────────────────

impl Store {
    pub async fn insert_worker(
        &self,
        worker: WorkerRoster,
        created_by: String,
    ) -> Result<WorkerRoster, StoreError> {
        self.run_named("insert_worker", move |conn| {
            let id = worker.meta.id.as_id().to_string();
            let now = crate::config::now_rfc3339();
            let doc = serde_json::to_string(&worker)?;
            conn.execute(
                "INSERT INTO worker_roster
                    (id, project_id, name, host_id, state_kind, doc_json, revision,
                     created_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?8)",
                params![
                    id,
                    worker.project_id.as_id().to_string(),
                    worker.name,
                    worker.host_id.as_id().to_string(),
                    worker.state.kind(),
                    doc,
                    created_by,
                    now,
                ],
            )?;
            load_worker(conn, &id)?.ok_or_else(|| StoreError::Id("worker insert missing".into()))
        })
        .await
    }

    pub async fn list_workers(
        &self,
        project_id: Option<String>,
    ) -> Result<Vec<WorkerRoster>, StoreError> {
        self.run_named("list_workers", move |conn| {
            let mut stmt = if project_id.is_some() {
                conn.prepare(
                    "SELECT id FROM worker_roster WHERE project_id = ?1 ORDER BY created_at",
                )?
            } else {
                conn.prepare("SELECT id FROM worker_roster ORDER BY created_at")?
            };
            // Bind the project id only on the parameterised query; binding a
            // NULL to the parameterless `else` statement is a rusqlite error.
            let ids: Vec<String> = match &project_id {
                Some(project_id) => stmt
                    .query_map(params![project_id], |row| row.get(0))?
                    .collect::<Result<_, _>>()?,
                None => stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<_, _>>()?,
            };
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_worker(conn, &id)?.ok_or_else(|| StoreError::Id("worker vanished".into()))
                })
                .collect()
        })
        .await
    }

    pub async fn get_worker(&self, id: String) -> Result<Option<WorkerRoster>, StoreError> {
        self.run_named("get_worker", move |conn| load_worker(conn, &id))
            .await
    }

    pub async fn find_active_workers_named(
        &self,
        name: String,
    ) -> Result<Vec<WorkerRoster>, StoreError> {
        self.run_named("find_active_workers_named", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM worker_roster WHERE name = ?1 AND state_kind != 'retired'",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![name], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_worker(conn, &id)?.ok_or_else(|| StoreError::Id("worker vanished".into()))
                })
                .collect()
        })
        .await
    }

    pub async fn list_workers_for_host(
        &self,
        host_id: String,
    ) -> Result<Vec<WorkerRoster>, StoreError> {
        self.run_named("list_workers_for_host", move |conn| {
            let mut stmt = conn.prepare("SELECT id FROM worker_roster WHERE host_id = ?1")?;
            let ids: Vec<String> = stmt
                .query_map(params![host_id], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_worker(conn, &id)?.ok_or_else(|| StoreError::Id("worker vanished".into()))
                })
                .collect()
        })
        .await
    }

    pub async fn mutate_worker<F>(
        &self,
        id: String,
        mutate: F,
    ) -> Result<Option<WorkerRoster>, StoreError>
    where
        F: FnOnce(&mut WorkerRoster) -> Result<(), StoreError> + Send + 'static,
    {
        self.run_named("mutate_worker", move |conn| {
            let Some(mut worker) = load_worker(conn, &id)? else {
                return Ok(None);
            };
            mutate(&mut worker)?;
            let now = crate::config::now_rfc3339();
            worker.meta.revision = U64(worker.meta.revision.0 + 1);
            worker.meta.updated_at = remuda_protocol::Timestamp::try_from(now.clone())
                .map_err(|err| StoreError::Id(err.to_string()))?;
            let doc = serde_json::to_string(&worker)?;
            conn.execute(
                "UPDATE worker_roster
                    SET doc_json = ?1, state_kind = ?2, revision = revision + 1, updated_at = ?3
                 WHERE id = ?4",
                params![doc, worker.state.kind(), now, id],
            )?;
            load_worker(conn, &id)
        })
        .await
    }
}

fn load_worker(conn: &Connection, id: &str) -> Result<Option<WorkerRoster>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM worker_roster WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(StoreError::from)
}

#[cfg(test)]
mod driver_choice_tests {
    use super::{driver_for, select_worker_driver};
    use crate::error::HubError;
    use crate::store::HostRecord;
    use serde_json::json;

    /// One host, described by what it says it can launch.
    ///
    /// `native` drives `capabilities.driverInventory[].launchable`, which is the
    /// only honest report of whether `shell-pty` can run an *agent* here: with
    /// `REMUDA_PTY_CARRIER` off the same descriptor arrives `launchable: false`,
    /// because `shell-pty` then launches a login shell instead.
    fn host(native: Option<bool>, herdr: bool) -> HostRecord {
        HostRecord {
            host_id: "hst_01hzzzzzzzzzzzzzzzzzzzzzzz".into(),
            label: "devbox".into(),
            state: "online".into(),
            online: true,
            last_seen_at: None,
            node_version: None,
            cli: json!([]),
            capabilities: match native {
                Some(launchable) => json!({
                    "driverInventory": [{
                        "kind": "shell-pty",
                        "launchable": launchable,
                        "reasonCode": if launchable { "carrier-native" } else { "carrier-not-enabled" },
                    }]
                }),
                // An older Node that cannot describe itself reports nothing at
                // all — absence means "not reported", never "cannot".
                None => json!({}),
            },
            instance_count: 0,
            transport: "ssh-stdio".into(),
            labels: Vec::new(),
            herdr: herdr.then(|| json!({"version": "0.9.0", "socket": "/tmp/h.sock"})),
            resources: None,
            max_instances: 8,
            hostname: None,
            ssh: None,
            last_error: None,
            provider_binding: "auto".into(),
            default_launch_args: None,
            claude_binary_path: None,
            default_tui: None,
            workspaces: Vec::new(),
            workspace_revision: 0,
        }
    }

    #[test]
    fn a_launchable_shell_pty_is_preferred_over_herdr() {
        // The native carrier serves a readable live screen, which is what
        // `remuda watch` reads; herdr's blit cannot scroll (D-028).
        let both = host(Some(true), true);
        assert_eq!(driver_for("claude", &both).unwrap(), "shell-pty");
        // codex/grok have only the herdr-backed generic pty driver.
        assert_eq!(driver_for("codex", &both).unwrap(), "generic-pty");
        let native_only = host(Some(true), false);
        assert_eq!(driver_for("claude", &native_only).unwrap(), "shell-pty");
    }

    #[test]
    fn herdr_only_falls_back_to_the_pty_drivers_not_to_print() {
        // The regression this pins: the old default answered `claude-print`
        // whenever herdr was missing, and picked `claude-pty` without ever
        // consulting the inventory.
        let reported_off = host(Some(false), true);
        assert_eq!(driver_for("claude", &reported_off).unwrap(), "claude-pty");
        assert_eq!(driver_for("codex", &reported_off).unwrap(), "generic-pty");
        // And never print, on any harness, however the inventory reads.
        for reported in [Some(false), None] {
            for herdr in [true, false] {
                for harness in ["claude", "codex", "grok"] {
                    if let Ok(driver) = driver_for(harness, &host(reported, herdr)) {
                        assert_ne!(driver, "claude-print", "{harness} {reported:?} {herdr}");
                    }
                }
            }
        }
        // A Node too old to report an inventory is not treated as a refusal.
        let unreported = host(None, true);
        assert_eq!(driver_for("claude", &unreported).unwrap(), "claude-pty");
    }

    #[test]
    fn neither_carrier_is_refused_rather_than_downgraded_to_print() {
        // `claude-print` exits after one turn, so a worker on it can never be
        // nudged, steered or watched. Refusing with a reason an operator can act
        // on beats launching a product nobody asked for.
        for host in [host(Some(false), false), host(None, false)] {
            let error = driver_for("claude", &host).expect_err("no carrier");
            let HubError::Unsatisfiable { reasons } = error else {
                panic!("expected Unsatisfiable, got {error:?}");
            };
            let reason = reasons.join(" ");
            assert!(reason.contains("REMUDA_PTY_CARRIER"), "{reason}");
            assert!(
                reason.contains("claude-print is never a default"),
                "{reason}"
            );
        }
        // codex/grok resolve to generic-pty unconditionally (batch 6), so they
        // are the one pair that never reaches this refusal.
        assert_eq!(
            driver_for("codex", &host(Some(false), false)).unwrap(),
            "generic-pty"
        );
    }

    #[test]
    fn an_unknown_harness_is_refused_rather_than_answered_with_a_carrier() {
        // Every harness must resolve to a real product or be refused; nothing
        // may be answered with a slot machine's default.
        let host = host(Some(true), true);
        assert_eq!(driver_for("claude", &host).unwrap(), "shell-pty");
        // A harness with no launch recipe is a BadRequest, not a silent claude
        // driver — `select_carrier` treats every non-claude kind as codex/grok,
        // so the refusal has to come from the explicit-driver path.
        let error =
            select_worker_driver("cursor", "shell-pty", &host).expect_err("unknown harness");
        assert!(
            matches!(&error, HubError::BadRequest(message) if message.contains("cursor")),
            "{error:?}"
        );
    }

    #[test]
    fn an_explicit_driver_is_honoured_verbatim() {
        let both = host(Some(true), true);
        for driver in ["shell-pty", "claude-pty", "claude-print"] {
            assert_eq!(
                select_worker_driver("claude", driver, &both).unwrap(),
                driver
            );
        }
        // print stays selectable for a scripted one-shot even where the native
        // carrier is available — it is just never reached by default.
        assert_eq!(
            select_worker_driver("claude", "claude-print", &host(Some(true), false)).unwrap(),
            "claude-print"
        );
    }

    #[test]
    fn an_explicit_driver_the_host_cannot_launch_is_refused_not_replaced() {
        // Silently substituting here is precisely the bug: the operator asked
        // for one product and a different one ran.
        let no_native = host(Some(false), true);
        let error = select_worker_driver("claude", "shell-pty", &no_native).expect_err("refused");
        assert!(
            matches!(&error, HubError::Conflict(message) if message.contains("REMUDA_PTY_CARRIER")),
            "{error:?}"
        );
        let no_herdr = host(Some(true), false);
        let error = select_worker_driver("claude", "claude-pty", &no_herdr).expect_err("refused");
        assert!(
            matches!(&error, HubError::Conflict(message) if message.contains("herdr")),
            "{error:?}"
        );
    }

    #[test]
    fn a_driver_the_harness_cannot_use_is_a_bad_request() {
        let both = host(Some(true), true);
        for (harness, driver) in [("codex", "claude-pty"), ("claude", "grok-acp")] {
            let error = select_worker_driver(harness, driver, &both).expect_err("invalid");
            assert!(
                matches!(&error, HubError::BadRequest(message) if message.contains(driver)),
                "{error:?}"
            );
        }
    }
}
