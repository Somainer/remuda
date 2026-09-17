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
use crate::agent_scope::{caller, caller_project_scope, require_grant};
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
    Ok(())
}

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
        .route("/v1/gate/jobs", get(list_all_jobs))
        .route("/v1/gate/logs/{id}", get(get_gate_log))
}

// ── request shapes ─────────────────────────────────────────────────────────

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
    let job = GateJob {
        id: GateJobId::new(),
        project_id,
        branch: branch.to_owned(),
        mode,
        web,
        lane_id: body.lane_id,
        requested_by: device.id.clone(),
        state: GateJobState::Queued,
        steps: Vec::new(),
        host_id: None,
        queued_at: ts(&now),
        started_at: None,
        finished_at: None,
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
        jobs.retain(|job| matches!(job.state, GateJobState::Queued | GateJobState::Running));
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
    match job.state {
        GateJobState::Queued => {
            let updated = state
                .store
                .mutate_gate_job(&job_id, |row| {
                    row.state = GateJobState::Canceled;
                    row.finished_at = Some(now_ts());
                    row.error = Some("canceled while queued".into());
                    Some(())
                })
                .await
                .map_err(map_store)?
                .ok_or(HubError::NotFound)?;
            journal(&state, &device.id, "gate.canceled", &updated).await;
            drop_job_refs(&state, &updated).await;
            Ok(Json(json!(updated)))
        }
        GateJobState::Running | GateJobState::Canceling => {
            let updated = state
                .store
                .mutate_gate_job(&job_id, |row| {
                    row.state = GateJobState::Canceling;
                    Some(())
                })
                .await
                .map_err(map_store)?
                .ok_or(HubError::NotFound)?;
            if let (Some(host), Some(lane)) = (&job.host_id, &job.lane_id) {
                let params = json!({ "jobId": job.id.as_id().as_str(), "laneId": lane });
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
        terminal => Err(HubError::Conflict(format!(
            "job already {terminal}",
            terminal = terminal.as_str()
        ))),
    }
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
    // Pinned-ref retention; self-throttled, so this is cheap on every tick.
    sweep_expired_refs(state).await;
}

async fn schedule(state: &AppState) -> Result<(), HubError> {
    let jobs = state.store.list_gate_jobs(None).await.map_err(map_store)?;
    if !jobs.iter().any(|job| job.state == GateJobState::Queued) {
        return Ok(());
    }
    // Lane usage from running jobs.
    let mut busy_lanes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut land_active: std::collections::HashSet<String> = std::collections::HashSet::new();
    for job in &jobs {
        if job.state == GateJobState::Running {
            if let Some(lane) = &job.lane_id {
                busy_lanes.insert(format!("{}:{lane}", job.project_id.as_id()));
            }
            if job.mode == GateMode::Land {
                land_active.insert(job.project_id.as_id().to_string());
            }
        }
    }
    // Group queued jobs per project in FIFO order; once an earlier queued job
    // cannot be dispatched this tick, later ones wait too (queue order).
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

async fn dispatch(
    state: &AppState,
    job: &GateJob,
    lane: &remuda_protocol::ProjectGateLane,
    base_branch: String,
    push_from: remuda_protocol::GatePushFrom,
) {
    let claim_lane = lane.id.clone();
    let claim_host = lane.host_id.clone();
    let claimed = state
        .store
        .mutate_gate_job(job.id.as_id().as_str(), move |row| {
            if row.state != GateJobState::Queued {
                return None;
            }
            row.state = GateJobState::Running;
            row.lane_id = Some(claim_lane.clone());
            row.host_id = Some(claim_host.clone());
            row.started_at = Some(now_ts());
            row.attempts = row.attempts.saturating_add(1);
            Some(())
        })
        .await;
    let Ok(Some(job)) = claimed else {
        return;
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
        timeouts: std::collections::BTreeMap::new(),
        gate_timeout_secs: 0,
        push: true,
        push_from,
        keep_logs: job.keep_logs,
        binary: None,
    };
    journal(state, &job.requested_by, "gate.running", &job).await;
    let state = state.clone();
    let job_id = job.id.as_id().to_string();
    let host_id = lane.host_id.clone();
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
                        apply_result(&state, &job_id, result).await;
                    }
                } else if let Some(error) = frame.get("error") {
                    fail_rpc(&state, &job_id, error).await;
                }
            }
            Ok(None) => {
                fail_rpc(
                    &state,
                    &job_id,
                    &json!({"message": "lane host is not connected"}),
                )
                .await;
            }
            Err(error) => fail_rpc(&state, &job_id, &json!({"message": error.to_string()})).await,
        }
        schedule_once(&state);
    });
}

async fn fail_rpc(state: &AppState, job_id: &str, error: &Value) {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("gate.run failed")
        .to_owned();
    let updated = state
        .store
        .mutate_gate_job(job_id, move |row| {
            if row.state != GateJobState::Running && row.state != GateJobState::Canceling {
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
}

/// Apply a `gate.event` Node notification (runs for both WSS and ssh-stdio
/// through `ws::handle_node_method`).
pub(crate) async fn on_event(
    state: &AppState,
    from_host: &str,
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
    match event.kind {
        Kind::Phase { .. } | Kind::Log { .. } => {}
        Kind::Step { step } => {
            let _ = state
                .store
                .mutate_gate_job(&job_id, |row| {
                    if row.state != GateJobState::Running && row.state != GateJobState::Canceling {
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
        Kind::Finished { result } => apply_result(state, &job_id, *result).await,
    }
    Ok(())
}

/// Apply the terminal verdict (event or RPC reply). Idempotent: only a running
/// job transitions.
async fn apply_result(state: &AppState, job_id: &str, result: GateRunResult) {
    // The terminal `finished` event and the `gate.run` reply carry the same
    // verdict; this read makes the second arrival a no-op before it would
    // replace the stored log object.
    let Some(existing) = state.store.get_gate_job(job_id).await.ok().flatten() else {
        return;
    };
    if !matches!(
        existing.state,
        GateJobState::Running | GateJobState::Canceling
    ) {
        return;
    }
    // Persist the bounded evidence out-of-band first; the job row keeps only
    // the `obj_…` reference. A persistence failure must not lose the verdict.
    let mut log_object_id: Option<String> = None;
    if let Some(run_log) = &result.run_log {
        match persist_run_log(
            state,
            existing.project_id.as_id().as_str(),
            job_id,
            &existing.requested_by,
            run_log,
        )
        .await
        {
            Ok(object_id) => log_object_id = Some(object_id),
            Err(error) => tracing::warn!(%error, job_id, "could not persist gate log object"),
        }
    }
    // A `pushFrom: home` land verifies on the lane and is pushed by the home
    // host. The lane therefore reports `passed` for a land job; the job stays
    // `running` across the handoff so `remuda land` keeps waiting instead of
    // reading a verify as a completed land.
    let home_land = if existing.mode == GateMode::Land && result.status == "passed" {
        home_land_plan(state, &existing, &result).await
    } else {
        None
    };
    let landing = home_land.clone();
    let outcome = state
        .store
        .mutate_gate_job(job_id, move |row| {
            if result.status == "canceled" {
                row.state = GateJobState::Canceled;
                row.finished_at = Some(now_ts());
                row.error = result.error.clone();
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
                row.state = GateJobState::Passed;
                row.finished_at = Some(now_ts());
            } else if result.status == "landed" {
                row.state = GateJobState::Landed;
                row.finished_at = Some(now_ts());
                row.error = None;
            } else if result.status == "base-moved"
                && row.mode == GateMode::Land
                && row.attempts < MAX_LAND_ATTEMPTS
            {
                row.state = GateJobState::Queued;
                row.started_at = None;
                row.host_id = None;
                row.lane_id = None;
                // The next attempt produces fresh evidence.
                row.failed_step = None;
                row.reason = None;
                row.log_object_id = None;
            } else {
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
        return;
    };
    // A claimed handoff: verified on the lane, now push from the home host.
    if let Some(plan) = home_land {
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
    // waiting for retention.
    if matches!(job.state, GateJobState::Canceled | GateJobState::Landed) {
        drop_job_refs(state, &job).await;
    }
    schedule_once(state);
    if job.state == GateJobState::Landed {
        run_then_hook(state, &job).await;
    }
}

/// Serialize and store the bounded run log as an `obj_…` gate log object.
/// One log per job: a later attempt replaces the previous row.
async fn persist_run_log(
    state: &AppState,
    project_id: &str,
    job_id: &str,
    created_by: &str,
    run_log: &remuda_protocol::GateRunLog,
) -> Result<String, HubError> {
    let bytes =
        serde_json::to_vec(run_log).map_err(|error| HubError::Internal(error.to_string()))?;
    if bytes.len() > GATE_LOG_MAX_BYTES {
        return Err(HubError::BadRequest(format!(
            "gate log is {} bytes; the limit is {GATE_LOG_MAX_BYTES}",
            bytes.len()
        )));
    }
    let digest = crate::config::sha256_hex(&bytes);
    state
        .store
        .insert_gate_log(project_id, job_id, bytes, digest, created_by)
        .await
        .map_err(HubError::Store)
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
        Ok(None) => remuda_protocol::GateLandResult {
            job_id: job_id.to_owned(),
            status: "failed".into(),
            error: Some("home host is not connected; nothing was pushed".into()),
            ..Default::default()
        },
        Err(error) => remuda_protocol::GateLandResult {
            job_id: job_id.to_owned(),
            status: "failed".into(),
            error: Some(format!("gate.land failed: {error}")),
            ..Default::default()
        },
    };
    let status = landed.status.clone();
    let outcome = state
        .store
        .mutate_gate_job(job_id, move |row| {
            if row.state != GateJobState::Running && row.state != GateJobState::Canceling {
                return None;
            }
            match landed.status.as_str() {
                "landed" => {
                    row.state = GateJobState::Landed;
                    row.finished_at = Some(now_ts());
                    row.merge_sha = landed.merge_sha.clone().or(row.merge_sha.clone());
                    row.error = None;
                    row.reason = None;
                }
                "base-moved" if row.attempts < MAX_LAND_ATTEMPTS => {
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
        .mutate_gate_job(job.id.as_id().as_str(), |row| {
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
        .mutate_gate_job(job.id.as_id().as_str(), move |row| {
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
    {
        let _ = state
            .store
            .mutate_gate_job(job.id.as_id().as_str(), |row| {
                row.state = GateJobState::Queued;
                row.started_at = None;
                row.host_id = None;
                row.lane_id = None;
                row.error = Some("hub restarted while the job was running; re-queued".into());
                Some(())
            })
            .await;
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
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO gate_jobs
                    (id, project_id, state, lane_id, queued_at, doc_json, revision,
                     created_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?8)",
                params![id, project_id, state, lane, queued, doc, "", created],
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
        self.run(move |conn| {
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

    pub(crate) async fn list_gate_jobs(
        &self,
        project_id: Option<&str>,
    ) -> Result<Vec<GateJob>, crate::store::StoreError> {
        let project_id = project_id.map(str::to_owned);
        self.run(move |conn| {
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
        F: FnOnce(&mut GateJob) -> Option<()> + Send + 'static,
    {
        let id = id.to_owned();
        self.run(move |conn| {
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
            if mutate(&mut job).is_none() {
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
}

// ── gate log objects ───────────────────────────────────────────────────────

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
    /// Replace and persist the log object for one job run; returns the new
    /// `obj_…` id.
    pub(crate) async fn insert_gate_log(
        &self,
        project_id: &str,
        job_id: &str,
        bytes: Vec<u8>,
        digest: String,
        created_by: &str,
    ) -> Result<String, crate::store::StoreError> {
        let project_id = project_id.to_owned();
        let job_id = job_id.to_owned();
        let created_by = created_by.to_owned();
        self.run(move |conn| {
            let now = now();
            let expires_at = gate_log_expiry(&now);
            // One log per job: a re-verify attempt replaces the prior row so
            // `remuda gate log <gjb>` always reads the latest run.
            conn.execute(
                "DELETE FROM gate_log_objects WHERE job_id = ?1",
                params![job_id],
            )?;
            let object_id = crate::config::new_id("obj")
                .map_err(|error| crate::store::StoreError::Id(error.to_string()))?;
            conn.execute(
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
            Ok(object_id)
        })
        .await
    }

    /// Load a gate log by either its `obj_…` id or the owning `gjb_…` job id.
    pub(crate) async fn get_gate_log(
        &self,
        id: &str,
    ) -> Result<Option<GateLogObject>, crate::store::StoreError> {
        let id = id.to_owned();
        self.run(move |conn| {
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
                conn.query_row(
                    "SELECT id, job_id, project_id, bytes, expires_at
                     FROM gate_log_objects WHERE job_id = ?1 ORDER BY rowid DESC LIMIT 1",
                    params![id],
                    mapper,
                )
                .optional()?
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
