//! D-062: claim an external herdr pane as an instance (`herdr-claimed`).
//!
//! - `GET /v1/hosts/{id}/herdr[?session=…]` → Node `herdr.discover`
//! - `POST /v1/instances/claim` → Node `herdr.resolve`, then `instance.claim`
//! - `POST /v1/instances/{id}/release` → detach (Node `instance.close`)
//!
//! Human operators only: an Agent, a Bot and a Human narrowed to an instance
//! get 403 with a reason and a next step. A claimed pane runs with a
//! permission mode and credentials Remuda never set, so letting anything but a
//! seated human claim, drive or release it would widen authority
//! (`docs/design/herdr-claim.md` §1). Release only detaches: the Node never
//! closes the pane, signals its process or stops its server.

use crate::agent_scope;
use crate::error::{NextStep, action};
use crate::http::map_store;
use crate::store::{Device, InstanceRecord};
use crate::{AppState, HubError};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use remuda_protocol::InputOrigin;
use remuda_protocol::hubnode::{
    HERDR_CLAIM_AMBIGUOUS, HERDR_CLAIM_CONFLICT, HERDR_CLAIM_INVALID, HERDR_CLAIM_OWN_SESSION,
    HERDR_UNREACHABLE, HerdrClaimResolution, METHOD_HERDR_DISCOVER, METHOD_HERDR_RESOLVE,
    METHOD_INSTANCE_CLAIM,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The driver every claimed instance carries.
pub(crate) const DRIVER: &str = "herdr-claimed";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/hosts/{id}/herdr", get(discover))
        .route("/v1/instances/claim", post(claim))
        .route("/v1/instances/{id}/release", post(release))
}

/// 403 for anyone but a seated human operator.
pub(crate) fn human_only() -> HubError {
    HubError::Forbidden.with_next(
        "human-only",
        NextStep::new(
            action::ASK_OWNER,
            "only a human operator may claim, drive or release an external herdr pane (D-062)",
        ),
    )
}

/// Whether `instance` is a claimed external pane.
pub(crate) fn is_claimed(instance: &InstanceRecord) -> bool {
    instance.driver == DRIVER
}

async fn require_human(state: &AppState, headers: &HeaderMap) -> Result<Device, HubError> {
    crate::auth::require_origin(headers, &state.config)?;
    // An Agent, or a Human narrowed to an instance, is refused here with
    // `operator-only`; a Bot passes this check and is refused below.
    let device = agent_scope::require_operator(state, headers).await?;
    if agent_scope::origin(&device) != InputOrigin::Human {
        return Err(human_only());
    }
    Ok(device)
}

async fn require_online_host(state: &AppState, host_id: &str) -> Result<(), HubError> {
    if state.store.get_host(host_id.to_owned()).await?.is_none() {
        return Err(HubError::NotFound);
    }
    if state.nodes.kind_of(host_id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: host_id.to_owned(),
        });
    }
    Ok(())
}

/// Map a Node claim refusal onto its HTTP status by the stable prefix the
/// Node puts in its error text.
fn claim_error(error: HubError) -> HubError {
    let HubError::BadRequest(message) = &error else {
        return error;
    };
    let detail = |prefix: &str| {
        message
            .find(prefix)
            .map_or_else(|| message.clone(), |at| message[at..].to_owned())
    };
    if message.contains(HERDR_CLAIM_CONFLICT) {
        return HubError::Conflict(detail(HERDR_CLAIM_CONFLICT)).with_next(
            "already-claimed",
            NextStep::new(
                action::READ_THEN_DECIDE,
                "release the instance that holds this pane before claiming it again",
            ),
        );
    }
    for (prefix, reason) in [
        (HERDR_CLAIM_AMBIGUOUS, "pane-ambiguous"),
        (HERDR_CLAIM_OWN_SESSION, "own-session"),
        (HERDR_CLAIM_INVALID, "invalid-target"),
        (HERDR_UNREACHABLE, "herdr-unreachable"),
    ] {
        if message.contains(prefix) {
            return HubError::BadRequest(detail(prefix)).with_next(
                reason,
                NextStep::new(
                    action::FIX_REQUEST,
                    "check the session, workspace, tab and pane with `remuda host herdr ls`",
                ),
            );
        }
    }
    error
}

#[derive(Deserialize)]
struct DiscoverQuery {
    #[serde(default)]
    session: Option<String>,
}

/// `GET /v1/hosts/{id}/herdr` — the host's reachable herdr sessions and their
/// panes. Metadata only; the Node never reads pane content for it.
async fn discover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
    Query(query): Query<DiscoverQuery>,
) -> Result<Json<Value>, HubError> {
    require_human(&state, &headers).await?;
    require_online_host(&state, &host_id).await?;
    let mut params = json!({});
    if let Some(session) = query.session.filter(|value| !value.trim().is_empty()) {
        params["session"] = json!(session);
    }
    let body = crate::http::call_node(&state, &host_id, METHOD_HERDR_DISCOVER, params)
        .await
        .map_err(claim_error)?;
    Ok(Json(body))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimBody {
    host_id: String,
    herdr_session: String,
    workspace_id: String,
    tab_id: String,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    /// Explicit scope (`{projectIds, hostIds, workspaceIds}`), as on create.
    #[serde(default)]
    scope: Option<Value>,
}

/// `POST /v1/instances/claim` — validate on the Node (read-only), index the
/// row, then commit the claim on the Node.
async fn claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ClaimBody>,
) -> Result<Json<Value>, HubError> {
    let device = require_human(&state, &headers).await?;
    for (field, value) in [
        ("hostId", &body.host_id),
        ("herdrSession", &body.herdr_session),
        ("workspaceId", &body.workspace_id),
        ("tabId", &body.tab_id),
    ] {
        if value.trim().is_empty() {
            return Err(HubError::BadRequest(format!("{field} is required")));
        }
    }
    let host_id = body.host_id.trim().to_owned();
    require_online_host(&state, &host_id).await?;
    let mut target = json!({
        "session": body.herdr_session.trim(),
        "workspaceId": body.workspace_id.trim(),
        "tabId": body.tab_id.trim(),
    });
    if let Some(pane) = body
        .pane_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        target["paneId"] = json!(pane);
    }
    if let Some(kind) = body
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        target["kind"] = json!(kind);
    }
    // 1. Read-only: the Node validates the id chain, the single-pane rule and
    //    its own uniqueness, and says what it would record. Nothing is
    //    written anywhere on refusal.
    let resolved = crate::http::call_node(&state, &host_id, METHOD_HERDR_RESOLVE, target.clone())
        .await
        .map_err(claim_error)?;
    let resolution: HerdrClaimResolution = serde_json::from_value(resolved)
        .map_err(|error| HubError::BadRequest(format!("node herdr.resolve: {error}")))?;
    let claim = &resolution.claim;
    // 2. One live claim per (host, socket, pane), also across Node restarts.
    if let Some(existing) = state
        .store
        .live_herdr_claim(
            host_id.clone(),
            claim.socket_path.clone(),
            claim.pane_id.clone(),
        )
        .await
        .map_err(map_store)?
    {
        return Err(HubError::Conflict(format!(
            "{HERDR_CLAIM_CONFLICT}: pane {} in herdr session {} is already claimed by {existing}",
            claim.pane_id, claim.session
        ))
        .with_next(
            "already-claimed",
            NextStep::new(
                action::READ_THEN_DECIDE,
                format!("release {existing} before claiming this pane again"),
            ),
        ));
    }
    // 3. Index the row before the Node creates the instance, so its first
    //    journal event lands on a row that already says herdr-claimed.
    let kind = serde_json::to_value(resolution.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "terminal".to_owned());
    let mut spec = json!({
        "kind": kind,
        "driver": DRIVER,
        // Owned by whoever started the pane; Remuda never invents one.
        "permissionMode": "external",
        "herdrClaim": claim,
    });
    if let Some(name) = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        spec["name"] = json!(name);
    }
    let title = body
        .title
        .clone()
        .or_else(|| body.name.clone())
        .filter(|value| !value.trim().is_empty());
    let mut delegation = crate::store::InstanceDelegation::default();
    if let Some(scope) = body.scope.clone() {
        if body.project_id.is_some() {
            return Err(HubError::BadRequest(
                "projectId is a single-project scope shortcut; put it in scope instead".into(),
            ));
        }
        delegation.scope = serde_json::from_value(scope)
            .map_err(|error| HubError::BadRequest(format!("invalid scope: {error}")))?;
    } else if let Some(project) = body.project_id.as_deref().filter(|v| !v.trim().is_empty()) {
        let project_id = remuda_protocol::ProjectId::try_from(project.trim().to_owned())
            .map_err(|error| HubError::BadRequest(format!("projectId: {error}")))?;
        delegation.scope = remuda_protocol::InstanceScope {
            project_ids: vec![project_id],
            ..Default::default()
        };
    }
    let record = state
        .store
        .insert_instance_delegated(
            host_id.clone(),
            None,
            kind.clone(),
            DRIVER.to_owned(),
            title,
            spec,
            delegation,
            agent_scope::CallerAuthority::internal(),
        )
        .await
        .map_err(map_store)?;
    // 4. Commit on the Node. A refusal here is a race with the read-only
    //    check (the pane vanished, another claim won): the row fails.
    target["instanceId"] = json!(record.instance_id);
    if let Err(error) = crate::http::call_node(&state, &host_id, METHOD_INSTANCE_CLAIM, target)
        .await
        .map_err(claim_error)
    {
        let _ = state
            .publish_settlement_unit(
                state
                    .store
                    .fail_instance(record.instance_id.clone(), "herdr-claim-refused".into()),
            )
            .await;
        return Err(error);
    }
    let activity = match resolution.agent_status.as_str() {
        "working" => "working",
        "blocked" => "blocked",
        _ => "idle",
    };
    state
        .store
        .mark_claimed(record.instance_id.clone(), activity.to_owned())
        .await
        .map_err(map_store)?;
    audit(
        &state,
        &device,
        "instance.claim",
        &record.instance_id,
        json!({
            "hostId": host_id,
            "session": claim.session,
            "socketPath": claim.socket_path,
            "workspaceId": claim.workspace_id,
            "tabId": claim.tab_id,
            "paneId": claim.pane_id,
            "kind": kind,
            "pid": claim.process.as_ref().map(|process| process.pid),
            "command": claim.process.as_ref().map(|process| process.command.clone()),
        }),
    )
    .await;
    let instance = state
        .store
        .get_instance(record.instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(json!({ "instance": instance, "claim": claim })))
}

/// `POST /v1/instances/{id}/release` — detach from a claimed pane. The pane,
/// its process and its herdr server keep running. Idempotent.
async fn release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let device = require_human(&state, &headers).await?;
    let instance = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    if !is_claimed(&instance) {
        return Err(HubError::BadRequest(format!(
            "instance {instance_id} is not a claimed herdr pane; close it instead"
        ))
        .with_next(
            "not-claimed",
            NextStep::new(
                action::FIX_REQUEST,
                "use `remuda instance rm` for this instance",
            ),
        ));
    }
    // A host-lost row may still be live on its Node: release it too, so the
    // Node detaches when it reconnects.
    let host_lost = instance.lifecycle == "exited"
        && instance.last_error.as_deref() == Some(crate::store::HOST_LOST_MARKER);
    if matches!(instance.lifecycle.as_str(), "exited" | "failed" | "closed") && !host_lost {
        return Ok(Json(json!({ "instance": instance, "released": false })));
    }
    let (command, created) = state
        .store
        .queue_command(
            None,
            Some(instance_id.clone()),
            instance.host_id.clone(),
            "instance.close".into(),
            json!({ "instanceId": instance_id, "origin": "human" }),
            None,
            None,
            None,
            None,
        )
        .await
        .map_err(map_store)?;
    if created {
        let live = state.nodes.kind_of(&instance.host_id).await.is_some();
        // The detach is delivered when the Node next connects; the release
        // itself never waits on the Node.
        if let Err(error) = crate::http::forward_if_online(&state, command, live).await {
            tracing::warn!(%instance_id, %error, "release not yet delivered to the node");
        }
    }
    state
        .publish_settlement(
            state
                .store
                .settle_instance_exited(instance_id.clone(), "released".into()),
        )
        .await
        .map_err(map_store)?;
    audit_release(&state, &device, &instance).await;
    let instance = state
        .store
        .get_instance(instance_id)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(json!({ "instance": instance, "released": true })))
}

/// Audit a release (also a close on a claimed instance through the commands
/// route). Ids only, never pane content.
pub(crate) async fn audit_release(state: &AppState, device: &Device, instance: &InstanceRecord) {
    let claim = instance.herdr_claim.clone().unwrap_or(Value::Null);
    audit(
        state,
        device,
        "instance.release",
        &instance.instance_id,
        json!({
            "hostId": instance.host_id,
            "session": claim.get("session"),
            "socketPath": claim.get("socketPath"),
            "workspaceId": claim.get("workspaceId"),
            "tabId": claim.get("tabId"),
            "paneId": claim.get("paneId"),
            "kind": instance.kind,
        }),
    )
    .await;
}

async fn audit(state: &AppState, device: &Device, action: &str, instance_id: &str, detail: Value) {
    if let Err(error) = state
        .store
        .append_audit(
            device.id.clone(),
            action.to_owned(),
            Some(instance_id.to_owned()),
            detail,
        )
        .await
    {
        tracing::warn!(%error, action, instance_id, "herdr claim audit row not written");
    }
}
