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
use crate::store::{ClaimDetach, Device, InstanceRecord};
use crate::{AppState, HubError};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use remuda_protocol::InputOrigin;
use remuda_protocol::hubnode::{
    HERDR_CLAIM_AMBIGUOUS, HERDR_CLAIM_CONFLICT, HERDR_CLAIM_INVALID, HERDR_CLAIM_NOT_READY,
    HERDR_CLAIM_OWN_SESSION, HERDR_UNREACHABLE, HerdrClaimResolution, METHOD_HERDR_DISCOVER,
    METHOD_HERDR_RESOLVE, METHOD_INSTANCE_CLAIM,
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

/// Operations that write to a claimed pane or end/release its claim (D-062
/// r2). On a `herdr-claimed` instance every one of them needs a seated Human
/// device, never a Bot, an Agent, a narrowed Human or the Hub itself.
pub(crate) const PANE_WRITE_OPS: &[&str] = &[
    "instance.send",
    "instance.keys",
    "tty.write",
    "tty.resize",
    "instance.configure",
    "instance.cancel",
    "instance.respond",
    "interaction.respond",
    "instance.close",
    "instance.resume",
    "instance.purge",
];

/// Hub-stamped proof, on a forwarded payload, that [`authorize_pane_write`]
/// admitted the write. `agent_scope::stamp` strips any caller-supplied copy.
pub(crate) const CLAIM_WRITE_MARK: &str = "claimWrite";

/// The ONE gate for writes to a claimed pane (D-062 r2 item 1).
///
/// Every Hub path that forwards a [`PANE_WRITE_OPS`] operation for an
/// instance calls this with the authenticated (narrowed) device, or `None`
/// for Hub-internal work. On a claimed instance only a Human device passes;
/// the payload is then stamped with [`CLAIM_WRITE_MARK`] and `origin: human`
/// so the forward-time check and the Node can verify it. Anything else is
/// 403 `human-only`. A non-claimed instance or another operation is untouched.
pub(crate) fn authorize_pane_write(
    instance: &InstanceRecord,
    device: Option<&Device>,
    operation: &str,
    payload: &mut Value,
) -> Result<(), HubError> {
    if !is_claimed(instance) || !PANE_WRITE_OPS.contains(&operation) {
        return Ok(());
    }
    let Some(device) = device.filter(|device| agent_scope::origin(device) == InputOrigin::Human)
    else {
        return Err(human_only());
    };
    if let Some(object) = payload.as_object_mut() {
        object.insert(CLAIM_WRITE_MARK.into(), json!(device.id));
        object.insert("origin".into(), json!("human"));
    }
    Ok(())
}

/// Forward-time backstop: a command for a claimed instance whose operation
/// writes to the pane must carry the gate's stamp, or it is never sent. A
/// route that forgot [`authorize_pane_write`] therefore fails closed.
pub(crate) async fn verify_forward(
    state: &AppState,
    command: &crate::store::CommandRecord,
) -> Result<(), HubError> {
    if !PANE_WRITE_OPS.contains(&command.operation.as_str()) {
        return Ok(());
    }
    let Some(instance_id) = command.instance_id.clone() else {
        return Ok(());
    };
    let Some(instance) = state.store.get_instance(instance_id).await? else {
        return Ok(());
    };
    if !is_claimed(&instance) {
        return Ok(());
    }
    if stamped(&command.payload) {
        Ok(())
    } else {
        Err(human_only())
    }
}

/// The direct-RPC twin of [`verify_forward`] (D-062 r3 item 4), run by
/// `node_ops::call_admitted_frame` before anything is sent: a pane write to a
/// claimed instance whose params lack the gate's stamp is refused.
pub(crate) async fn verify_direct(
    state: &AppState,
    method: &str,
    params: &Value,
) -> Result<(), HubError> {
    if !PANE_WRITE_OPS.contains(&method) {
        return Ok(());
    }
    let Some(instance_id) = params.get("instanceId").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(instance) = state.store.get_instance(instance_id.to_owned()).await? else {
        return Ok(());
    };
    if !is_claimed(&instance) || stamped(params) {
        return Ok(());
    }
    Err(human_only())
}

/// Whether a payload carries [`authorize_pane_write`]'s stamp.
fn stamped(payload: &Value) -> bool {
    payload
        .get(CLAIM_WRITE_MARK)
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
        && payload.get("origin").and_then(Value::as_str) == Some("human")
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
    if message.contains(HERDR_CLAIM_NOT_READY) {
        return HubError::Conflict(detail(HERDR_CLAIM_NOT_READY)).with_next(
            "not-ready",
            NextStep::new(
                action::RETRY_SAME_COMMAND,
                "start the agent in the pane (herdr must report it in the foreground), then claim again",
            ),
        );
    }
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
            // D-062 r2 item 6: file under the workspace holding the pane.
            resolution.remuda_workspace_id.clone(),
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
        // D-062 r2 item 4: the Node may have committed the claim and only the
        // reply was lost. Queue its idempotent detach (a Node without the
        // claim refuses it harmlessly), so no claim is left the Hub cannot
        // release.
        if let Err(detach) =
            queue_detach(&state, &device, &record, "claim-commit-unconfirmed").await
        {
            tracing::warn!(instance_id = %record.instance_id, %detach, "claim detach not queued");
        }
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
        // D-062 r3 item 1: an ended or failed claim row may still be live on
        // its Node (a claim whose commit reply was lost, a row from before
        // the detach existed). What decides is the detach's delivery, never
        // the release audit: one already delivered makes this a no-op, one
        // still queued is driven now, and with none at all one is queued.
        let detach = if instance.herdr_claim.is_some() {
            state
                .store
                .claim_detach_state(instance_id.clone())
                .await
                .map_err(map_store)?
        } else {
            ClaimDetach::Delivered
        };
        let released = detach == ClaimDetach::Missing;
        if released {
            queue_detach(&state, &device, &instance, "ended-without-release").await?;
        } else if detach == ClaimDetach::Pending {
            deliver_detaches(&state, &instance.host_id).await;
        }
        let instance = state
            .store
            .get_instance(instance_id)
            .await?
            .ok_or(HubError::NotFound)?;
        return Ok(Json(json!({ "instance": instance, "released": released })));
    }
    queue_detach(&state, &device, &instance, "released").await?;
    state
        .publish_settlement(
            state
                .store
                .settle_instance_exited(instance_id.clone(), "released".into()),
        )
        .await
        .map_err(map_store)?;
    let instance = state
        .store
        .get_instance(instance_id)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(json!({ "instance": instance, "released": true })))
}

/// Queue the Node detach for a claim (an idempotent `instance.close`),
/// delivered now or, while the host is away, by [`deliver_detaches`] on its
/// next hello or the Hub tick. The release audit records the human's request
/// and names the detach command; whether the detach was delivered is that
/// command's state (`Store::claim_detach_state`), never the audit. The
/// release itself never waits on the Node.
async fn queue_detach(
    state: &AppState,
    device: &Device,
    instance: &InstanceRecord,
    reason: &str,
) -> Result<(), HubError> {
    let mut payload = json!({ "instanceId": instance.instance_id, "origin": "human" });
    authorize_pane_write(instance, Some(device), "instance.close", &mut payload)?;
    let (command, created) = state
        .store
        .queue_command(
            None,
            Some(instance.instance_id.clone()),
            instance.host_id.clone(),
            "instance.close".into(),
            payload,
            None,
            None,
            None,
            None,
        )
        .await
        .map_err(map_store)?;
    let command_id = command.command_id.clone();
    if created {
        let live = state.nodes.kind_of(&instance.host_id).await.is_some();
        if let Err(error) = crate::http::forward_if_online(state, command, live).await {
            tracing::warn!(instance_id = %instance.instance_id, %error, "claim detach not yet delivered to the node");
        }
    }
    audit_release_with(state, device, instance, Some(reason), Some(&command_id)).await;
    Ok(())
}

/// D-062 r3 item 1: forward every claim detach still queued for `host_id`.
/// Runs after the host's hello and from the Hub tick, so a detach queued
/// while the Node was away (or whose send never left the Hub) reaches it.
/// Each command is sent at most once: `forward_if_online` serializes attempts
/// per command and its durable forward mark refuses a second send.
pub(crate) async fn deliver_detaches(state: &AppState, host_id: &str) {
    if state.nodes.kind_of(host_id).await.is_none() {
        return;
    }
    let pending = match state
        .store
        .undelivered_claim_detaches(Some(host_id.to_owned()))
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%host_id, %error, "queued claim detaches not read");
            return;
        }
    };
    for command in pending {
        let command_id = command.command_id.clone();
        if let Err(error) = crate::http::forward_if_online(state, command, true).await {
            tracing::warn!(%host_id, %command_id, %error, "queued claim detach not delivered");
        }
    }
}

/// The Hub tick's half of [`deliver_detaches`]: every online host with a
/// queued detach. Skipped while a previous pass still runs.
pub(crate) async fn deliver_all_detaches(state: &AppState) {
    let Ok(_pass) = state.herdr_detaches.try_lock() else {
        return;
    };
    let pending = match state.store.undelivered_claim_detaches(None).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%error, "queued claim detaches not read");
            return;
        }
    };
    let mut hosts: Vec<String> = pending.into_iter().map(|command| command.host_id).collect();
    hosts.sort();
    hosts.dedup();
    for host_id in hosts {
        deliver_detaches(state, &host_id).await;
    }
}

/// Audit a release (also a close on a claimed instance through the commands
/// route). Ids only, never pane content.
pub(crate) async fn audit_release(state: &AppState, device: &Device, instance: &InstanceRecord) {
    audit_release_with(state, device, instance, None, None).await;
}

async fn audit_release_with(
    state: &AppState,
    device: &Device,
    instance: &InstanceRecord,
    reason: Option<&str>,
    command_id: Option<&str>,
) {
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
            "reason": reason,
            "commandId": command_id,
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

#[cfg(test)]
mod backstop {
    //! D-062 r3 item 4: the direct-RPC backstop in `call_admitted_frame`.
    use super::{DRIVER, verify_direct};
    use serde_json::json;

    #[tokio::test]
    async fn an_unstamped_direct_pane_write_to_a_claimed_row_is_never_sent() {
        let dir = tempfile::tempdir().expect("dir");
        let hub = crate::spawn(crate::HubConfig::for_test(dir.path().join("data")))
            .await
            .expect("hub");
        let state = hub.test_state().clone();
        let host = remuda_protocol::HostId::new().as_id().as_str().to_owned();
        hub.test_insert_host(&host).await.expect("host");
        let claimed = state
            .store
            .insert_instance_delegated(
                host.clone(),
                None,
                "claude".into(),
                DRIVER.into(),
                None,
                json!({"kind": "claude", "driver": DRIVER}),
                crate::store::InstanceDelegation::default(),
                crate::agent_scope::CallerAuthority::internal(),
            )
            .await
            .expect("claimed row");
        let id = claimed.instance_id;
        let unstamped = json!({"instanceId": id});
        let refused = verify_direct(&state, "instance.send", &unstamped)
            .await
            .expect_err("an unstamped pane write is refused");
        assert_eq!(refused.status_code_and_body().0, 403, "{refused}");
        // Through the helper itself: refused before the host check, so the
        // answer is the 403, not host-offline.
        let through = crate::node_ops::call_admitted_frame(
            &state,
            &host,
            "tty.write",
            None,
            "op_test".into(),
            json!({"instanceId": id, "dataBase64": "eA=="}),
            std::time::Duration::from_secs(1),
            &crate::node_ops::NodeOpAuth {
                initiator: None,
                device_id: None,
            },
        )
        .await
        .expect_err("refused");
        assert_eq!(through.status_code_and_body().0, 403, "{through}");
        // Stamped by the gate, a read, or another instance: untouched.
        let stamped = json!({"instanceId": id, "claimWrite": "dev_x", "origin": "human"});
        verify_direct(&state, "instance.send", &stamped)
            .await
            .expect("stamped");
        verify_direct(&state, "tty.screen", &unstamped)
            .await
            .expect("a read");
        verify_direct(&state, "instance.send", &json!({"instanceId": "ins_other"}))
            .await
            .expect("not a claimed row");
        hub.shutdown().await;
    }
}

#[cfg(test)]
mod gate_scan {
    //! D-062 r2 item 1 / r3 item 4, structural: every Hub path that sends a
    //! Node RPC able to write to a claimed pane (or end its claim) goes through
    //! the Human-only gate. A new route that forwards one without it fails here.
    //!
    //! What counts as a send:
    //! * any `.call(` in production code, whatever its receiver is named: in
    //!   the Hub that is a Node transport (`ConnectedNodes::call(host, method,
    //!   params, timeout)`, or a pinned / raw transport's `call(method, params,
    //!   timeout)`), so the method is argument 1 of a 4-argument call and
    //!   argument 0 of a 3-argument one;
    //! * a call of a generic RPC helper ([`HELPERS`]), at its method argument.
    //!
    //! A method that may be a pane write is a pane-write literal, a protocol
    //! constant naming one, or anything that is not a literal or a constant (a
    //! variable carries any method). The calling function must run the gate.
    use super::PANE_WRITE_OPS;
    use std::path::{Path, PathBuf};

    /// Hub→Node RPC helpers that send the method their caller names: `(file
    /// defining it, fn, index of the method argument, private)`. A private
    /// helper's call sites are only looked for in its own file. Their bodies
    /// are exempt (the method is the caller's argument), so the rule applies
    /// at their call sites instead; `call_admitted_frame` also runs
    /// [`super::verify_direct`] itself.
    const HELPERS: &[(&str, &str, usize, bool)] = &[
        ("http.rs", "call_node", 2, false),
        ("http.rs", "call_node_pinned", 2, false),
        ("host_files.rs", "proxy_to_host", 3, true),
        ("workspaces.rs", "proxy_scm", 3, true),
        // `workspace.register` / `workspace.unregister`.
        ("workspaces.rs", "mutate", 4, true),
        ("node_ops.rs", "call_admitted", 2, false),
        ("node_ops.rs", "call_admitted_frame", 2, false),
    ];
    /// In-process test seams (`#[doc(hidden)]`, never routed) that pass a
    /// caller-chosen method to `call_admitted_frame`, whose
    /// [`super::verify_direct`] refuses an unstamped pane write. Each must
    /// still exist and still go through that helper.
    const SEAMS: &[(&str, &str)] = &[("lib.rs", "test_call_node_op")];
    /// Gate calls: the Human-only gate and its two backstops.
    const GATES: &[&str] = &["authorize_pane_write(", "verify_forward(", "verify_direct("];
    /// The transport layer itself routes nothing.
    const SKIPPED_FILES: &[&str] = &["transport.rs"];

    #[derive(Default, Debug)]
    struct Scan {
        /// `file::fn (method)` of every send that may be a pane write.
        checked: Vec<String>,
        /// Those whose function runs no gate.
        offenders: Vec<String>,
    }

    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Production text: the file without its `#[cfg(test)]` items (each
    /// skipped through its `;` or its balanced `{…}` block), so production
    /// code AFTER a test item is still scanned.
    fn production(text: &str) -> String {
        const ATTR: &str = "#[cfg(test)]";
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(ATTR) {
            out.push_str(&rest[..at]);
            let item = &rest[at + ATTR.len()..];
            rest = &item[item_len(item)..];
        }
        out.push_str(rest);
        out
    }

    /// Byte length of the item at the start of `text`: through the first
    /// top-level `;`, or through the `}` closing its first block. Strings, raw
    /// strings, char literals and comments are stepped over.
    fn item_len(text: &str) -> usize {
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i += 1;
                }
                b'r' if matches!(bytes.get(i + 1), Some(b'"' | b'#'))
                    && !bytes[..i]
                        .last()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') =>
                {
                    let hashes = bytes[i + 1..].iter().take_while(|c| **c == b'#').count();
                    if bytes.get(i + 1 + hashes) == Some(&b'"') {
                        let close = format!("\"{}", "#".repeat(hashes));
                        let from = i + 2 + hashes;
                        i = text[from..]
                            .find(&close)
                            .map_or(bytes.len(), |end| from + end + close.len())
                            - 1;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                // A char literal ('x', '\n', '\''), never a lifetime ('a).
                b'\'' if bytes.get(i + 1) == Some(&b'\\') => {
                    i += 3;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                }
                b'\'' if bytes.get(i + 2) == Some(&b'\'') => i += 2,
                b';' if depth == 0 => return i + 1,
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        bytes.len()
    }

    /// `(file name, production text)` of every Hub source.
    fn hub_sources() -> Vec<(String, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        files
            .into_iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_owned();
                let text = std::fs::read_to_string(&path).expect("source");
                (name, production(&text))
            })
            .collect()
    }

    /// `(name, start, end)` of every fn in `text`, by the next fn header.
    fn functions(text: &str) -> Vec<(String, usize, usize)> {
        let mut starts = Vec::new();
        let mut search = 0;
        while let Some(found) = text[search..].find("fn ") {
            let at = search + found;
            search = at + 3;
            let prev = text[..at].chars().last().unwrap_or('\n');
            if !(prev.is_whitespace()) {
                continue;
            }
            let name: String = text[at + 3..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                starts.push((name, at));
            }
        }
        let mut out = Vec::new();
        for (index, (name, start)) in starts.iter().enumerate() {
            let end = starts.get(index + 1).map_or(text.len(), |next| next.1);
            out.push((name.clone(), *start, end));
        }
        out
    }

    /// Top-level arguments of the call opening at `open` (`(`).
    fn arguments(text: &str, open: usize) -> Vec<String> {
        let mut depth = 0usize;
        let mut current = String::new();
        let mut args = Vec::new();
        let mut in_str = false;
        let mut escaped = false;
        let mut in_comment = false;
        let mut chars = text[open + 1..].chars().peekable();
        while let Some(ch) = chars.next() {
            if in_comment {
                in_comment = ch != '\n';
                continue;
            }
            if !in_str && ch == '/' && chars.peek() == Some(&'/') {
                in_comment = true;
                continue;
            }
            if in_str {
                current.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_str = false;
                }
                continue;
            }
            match ch {
                '"' => {
                    in_str = true;
                    current.push(ch);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    current.push(ch);
                }
                ')' | ']' | '}' if depth == 0 => {
                    if !current.trim().is_empty() {
                        args.push(current.trim().to_owned());
                    }
                    break;
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    current.push(ch);
                }
                ',' if depth == 0 => {
                    args.push(current.trim().to_owned());
                    current.clear();
                }
                _ => current.push(ch),
            }
        }
        args
    }

    /// Whether a method argument can name a pane write: a guarded literal, a
    /// constant naming one, or anything that is not a literal or a constant.
    fn may_write(method: &str) -> bool {
        let method = method.trim().trim_start_matches('&');
        if let Some(literal) = method
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
        {
            return PANE_WRITE_OPS.contains(&literal);
        }
        let last = method.rsplit("::").next().unwrap_or(method);
        let constant = !last.is_empty()
            && last
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !constant {
            // A variable, a field or an expression carries any method.
            return true;
        }
        PANE_WRITE_OPS.iter().any(|op| {
            let named = op.replace('.', "_").to_ascii_uppercase();
            last.contains(&named)
        })
    }

    /// Whether the match at `at` starts an identifier (not `xcall_node(`).
    fn starts_identifier(body: &str, at: usize) -> bool {
        !body[..at]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    }

    fn scan(files: &[(String, String)]) -> Scan {
        let mut out = Scan::default();
        for (file, text) in files {
            if SKIPPED_FILES.contains(&file.as_str()) {
                continue;
            }
            for (function, start, end) in functions(text) {
                if HELPERS
                    .iter()
                    .any(|(home, name, _, _)| *home == file.as_str() && *name == function)
                    || SEAMS.contains(&(file.as_str(), function.as_str()))
                {
                    continue;
                }
                let body = &text[start..end];
                let gated = GATES.iter().any(|gate| body.contains(gate));
                let mut sends: Vec<(String, String)> = Vec::new();
                // Any `.call(`: a Node transport call by its arity.
                let mut search = 0;
                while let Some(found) = body[search..].find(".call(") {
                    let at = search + found;
                    search = at + ".call(".len();
                    let args = arguments(body, at + ".call(".len() - 1);
                    let method = match args.len() {
                        0..=2 => continue,
                        3 => args[0].clone(),
                        4 => args[1].clone(),
                        _ => "<unknown arity>".to_owned(),
                    };
                    sends.push((".call".to_owned(), method));
                }
                for (home, helper, method_arg, private) in HELPERS {
                    if *private && *home != file.as_str() {
                        continue;
                    }
                    let call = format!("{helper}(");
                    let mut search = 0;
                    while let Some(found) = body[search..].find(&call) {
                        let at = search + found;
                        search = at + call.len();
                        if !starts_identifier(body, at) || body[..at].ends_with("fn ") {
                            continue;
                        }
                        let args = arguments(body, at + call.len() - 1);
                        let method = args
                            .get(*method_arg)
                            .cloned()
                            .unwrap_or_else(|| "<missing method>".to_owned());
                        sends.push(((*helper).to_owned(), method));
                    }
                }
                for (via, method) in sends {
                    if !may_write(&method) {
                        continue;
                    }
                    out.checked.push(format!("{file}::{function} ({method})"));
                    if !gated {
                        out.offenders
                            .push(format!("{file}::{function} sends {method} via {via}"));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_node_write_path_runs_the_human_only_gate() {
        let files = hub_sources();
        let found = scan(&files);
        assert!(
            found.offenders.is_empty(),
            "Node write paths that skip herdr_claims::authorize_pane_write / verify_forward / verify_direct: {:#?}",
            found.offenders
        );
        // The scan is not vacuous: it saw the known write paths.
        for known in ["forward_if_online", "forward_tty_call", "delete_instance"] {
            assert!(
                found
                    .checked
                    .iter()
                    .any(|site| site.contains(&format!("::{known} "))),
                "{known} not found by the scan: {:#?}",
                found.checked
            );
        }
        let text = |file: &str| {
            files
                .iter()
                .find(|(name, _)| name == file)
                .map(|(_, text)| text.clone())
                .unwrap_or_else(|| panic!("{file}"))
        };
        let body = |file: &str, function: &str| {
            let text = text(file);
            let (_, start, end) = functions(&text)
                .into_iter()
                .find(|(name, _, _)| name == function)
                .unwrap_or_else(|| panic!("{file}::{function}"));
            text[start..end].to_owned()
        };
        assert!(
            body("http.rs", "forward_if_online").contains("herdr_claims::verify_forward("),
            "forward_if_online must keep the forward-time backstop"
        );
        assert!(
            body("node_ops.rs", "call_admitted_frame").contains("herdr_claims::verify_direct("),
            "call_admitted_frame must keep the direct-RPC backstop"
        );
        // Production code after a file's first test item is scanned too.
        for (file, late) in [
            ("lib.rs", "fn expire_stale_requested"),
            ("gatequeue.rs", "fn spawn_landing_reconciler"),
            ("workspaces.rs", "fn mutate"),
        ] {
            assert!(text(file).contains(late), "{file}: {late} not scanned");
        }
        // Every exemption still names a real function, and every seam still
        // goes through the backstopped helper.
        for (file, function, _, _) in HELPERS {
            body(file, function);
        }
        for (file, function) in SEAMS {
            assert!(
                body(file, function).contains("call_admitted_frame("),
                "{file}::{function} must send through call_admitted_frame"
            );
        }
    }

    /// RED fixtures: each ungated send shape is caught; gated or read-only
    /// sends are not.
    #[test]
    fn the_scan_catches_every_ungated_send_shape() {
        let fixture = |text: &str| scan(&[("fixture.rs".to_owned(), text.to_owned())]);
        let caught = [
            // A route using call_admitted for instance.send.
            r#"async fn sneaky_send(state: AppState) {
                crate::node_ops::call_admitted(&state, &host, "instance.send", None, op, json!({"instanceId": id}), timeout, &auth).await;
            }"#,
            r#"async fn sneaky_frame(state: AppState) {
                call_admitted_frame(&state, &host, "tty.write", None, op, params, timeout, &auth).await;
            }"#,
            // A non-literal method at a helper call site.
            r#"async fn relay(state: AppState, method: &'static str) {
                crate::node_ops::call_admitted_frame(&state, &host, method, None, op, params, timeout, &auth).await;
            }"#,
            r#"async fn relay_node(state: AppState, method: String) {
                crate::http::call_node(&state, &host, &method, params).await;
            }"#,
            // A Node transport under another name.
            r#"async fn aliased(state: AppState) {
                let link = &state.nodes;
                link.call(&host, "instance.keys", params, timeout).await;
            }"#,
            r#"async fn pinned_write(pin: NodePin) {
                pin.call("instance.close", params, timeout).await;
            }"#,
            r#"async fn constant(state: AppState) {
                state.nodes.call(&host, remuda_protocol::hubnode::METHOD_TTY_WRITE, params, timeout).await;
            }"#,
            r#"async fn variable_op(state: AppState, op: &str) {
                transport.call(op, params, timeout).await;
            }"#,
            // A comment inside the arguments does not hide the method.
            r#"async fn commented(state: AppState) {
                state.nodes.call(
                    &host,
                    "instance.purge",
                    params,
                    // wait for the driver, then drop it, so allow more time
                    timeout,
                ).await;
            }"#,
        ];
        // Production code after a test item is still scanned.
        let after_tests = production(
            r#"#[cfg(test)]
            mod early_tests {
                fn helper() { let quote = '"'; let brace = '{'; let s = "}"; }
            }
            #[cfg(test)]
            mod declared;
            async fn late_route(state: AppState) {
                state.nodes.call(&host, "tty.write", params, timeout).await;
            }"#,
        );
        assert!(!after_tests.contains("early_tests") && !after_tests.contains("declared"));
        let found = fixture(&after_tests);
        assert_eq!(found.offenders.len(), 1, "{after_tests}\n{found:?}");
        for text in caught {
            let found = fixture(text);
            assert_eq!(
                found.offenders.len(),
                1,
                "an ungated send was missed: {text}\n{found:?}"
            );
        }
        let passed = [
            // The same send behind the gate.
            r#"async fn gated(state: AppState) {
                crate::herdr_claims::authorize_pane_write(&instance, Some(&device), "instance.send", &mut params)?;
                crate::node_ops::call_admitted(&state, &host, "instance.send", None, op, params, timeout, &auth).await;
            }"#,
            // Read-only methods and constants.
            r#"async fn lease(state: AppState) {
                crate::node_ops::call_admitted(&state, &host, "worktree.lease", None, op, params, timeout, &auth).await;
                state.nodes.call(&host, METHOD_HERDR_DISCOVER, json!({}), timeout).await;
                crate::http::call_node(&state, &host, METHOD_HERDR_RESOLVE, params).await;
            }"#,
            // Not a Node transport call: too few arguments.
            r#"fn service(svc: S) { svc.call(request); }"#,
        ];
        for text in passed {
            let found = fixture(text);
            assert!(
                found.offenders.is_empty(),
                "a gated or read-only send was flagged: {text}\n{found:?}"
            );
        }
    }
}
