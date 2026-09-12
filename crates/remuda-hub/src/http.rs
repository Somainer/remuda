//! REST surface used by the Web client.

use crate::AppState;
use crate::auth::{device_cookie, hash_secret, require_device, require_origin};
use crate::error::HubError;
use crate::store::CommandRecord;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginBody {
    bootstrap_token: String,
    #[serde(default = "default_device_name")]
    device_name: String,
}

fn default_device_name() -> String {
    "device".to_string()
}

#[derive(Deserialize)]
pub struct InstanceListQuery {
    #[serde(rename = "hostId")]
    host_id: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateInstanceBody {
    #[serde(rename = "hostId")]
    host_id: Option<String>,
    #[serde(rename = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default = "default_driver")]
    driver: String,
    title: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    placement: Option<Value>,
    #[serde(default)]
    delegation: Option<String>,
}

fn default_kind() -> String {
    "claude".into()
}
fn default_driver() -> String {
    "claude-print".into()
}

#[derive(Deserialize)]
pub struct CommandBody {
    #[serde(rename = "commandId")]
    command_id: Option<String>,
    #[serde(default = "default_operation")]
    operation: String,
    #[serde(default)]
    payload: Value,
    #[serde(rename = "idempotencyKey")]
    idempotency_key: Option<String>,
}

fn default_operation() -> String {
    "instance.send".into()
}

/// REST routes (login, hosts, instances, journal). Placement/fleet merge later.
pub fn routes() -> Router<crate::AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/login", post(login))
        .route("/v1/hosts", get(list_hosts))
        .route("/v1/instances", get(list_instances).post(create_instance))
        .route("/v1/instances/{id}", get(get_instance))
        .route("/v1/instances/{id}/commands", post(post_command))
        .route("/v1/instances/{id}/journal", get(get_journal))
}

#[derive(Deserialize)]
pub struct JournalQuery {
    #[serde(rename = "afterSeq")]
    after_seq: Option<String>,
}

/// `GET /healthz`
pub async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// `POST /v1/login` — bootstrap token → device token + cookie.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    if !crate::config::secret_eq(&body.bootstrap_token, &state.config.bootstrap_token) {
        return Err(HubError::Unauthenticated);
    }
    let token = crate::config::random_token();
    let hash = hash_secret(&token)?;
    let device = state.store.insert_device(body.device_name, hash).await?;
    let cookie = device_cookie(&token, state.config.cookie_secure);
    let body = json!({
        "deviceId": device.id,
        "token": token,
        "name": device.name,
    });
    Ok((StatusCode::OK, [(header::SET_COOKIE, cookie)], Json(body)).into_response())
}

/// `GET /v1/hosts`
pub async fn list_hosts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    let items = state.store.list_hosts().await?;
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `GET /v1/instances`
pub async fn list_instances(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InstanceListQuery>,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    let items = state.store.list_instances(query.host_id).await?;
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `GET /v1/instances/:id`
pub async fn get_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    let instance = state
        .store
        .get_instance(instance_id)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(
        serde_json::to_value(instance).map_err(|err| HubError::Internal(err.to_string()))?,
    ))
}

/// `POST /v1/instances` — index + forward `instance.create` when the Node is online.
pub async fn create_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateInstanceBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let mut spec = json!({
        "kind": body.kind,
        "driver": body.driver,
        "workspaceId": body.workspace_id,
        "prompt": body.prompt,
        "title": body.title,
    });
    if let Some(delegation) = &body.delegation
        && let Some(obj) = spec.as_object_mut()
    {
        obj.insert("delegation".into(), json!(delegation));
    }
    let placement =
        crate::placement::Placement::from_value(body.placement.as_ref(), body.host_id.as_deref())?;
    let place_spec = crate::placement::PlaceSpec::from_json(&spec);
    let host = crate::placement::pick_hosts(&state, &placement, &place_spec)
        .await?
        .into_iter()
        .next()
        .ok_or(HubError::Unsatisfiable {
            reasons: vec!["placement returned no host".into()],
        })?;
    if let Some(obj) = spec.as_object_mut() {
        obj.insert("hostId".into(), json!(host.host_id));
    }
    let (instance, command) = crate::placement::spawn_on_host(
        &state,
        &host,
        crate::placement::SpawnRequest {
            kind: body.kind,
            driver: body.driver,
            workspace_id: body.workspace_id,
            title: body.title,
            prompt: body.prompt,
            spec,
        },
    )
    .await?;
    Ok(Json(
        json!({ "instance": instance, "command": command, "hostId": host.host_id }),
    ))
}

/// `POST /v1/instances/:id/commands`
pub async fn post_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Json(body): Json<CommandBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let instance = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let mut payload = body.payload;
    if payload.is_null() {
        payload = json!({});
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("instanceId".to_string())
            .or_insert_with(|| json!(instance_id.clone()));
    }
    let (command, created) = state
        .store
        .queue_command(
            body.command_id,
            Some(instance_id),
            instance.host_id.clone(),
            body.operation,
            payload,
            body.idempotency_key,
        )
        .await
        .map_err(map_store)?;
    if !created {
        return Ok(Json(json!({ "command": command, "replayed": true })));
    }
    let online = state
        .store
        .get_host(instance.host_id)
        .await?
        .map(|h| h.online)
        .unwrap_or(false);
    let command = forward_if_online(&state, command, online).await?;
    Ok(Json(json!({ "command": command, "replayed": false })))
}

/// `GET /v1/instances/:id/journal`
pub async fn get_journal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Query(query): Query<JournalQuery>,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    if state
        .store
        .get_instance(instance_id.clone())
        .await?
        .is_none()
    {
        return Err(HubError::NotFound);
    }
    let after = query
        .after_seq
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let (events, durable) = state.store.read_journal(instance_id.clone(), after).await?;
    Ok(Json(json!({
        "instanceId": instance_id,
        "durableSeq": durable.to_string(),
        "events": events,
    })))
}

pub(crate) async fn forward_if_online(
    state: &AppState,
    command: CommandRecord,
    host_online: bool,
) -> Result<CommandRecord, HubError> {
    if !host_online {
        return Ok(command);
    }
    let first = state
        .store
        .mark_forward_intent(command.command_id.clone())
        .await?;
    if !first {
        return state
            .store
            .get_command(command.command_id.clone())
            .await?
            .ok_or(HubError::NotFound);
    }
    match state
        .nodes
        .call(
            &command.host_id,
            &command.operation,
            command.payload.clone(),
            Duration::from_secs(5),
        )
        .await
    {
        Ok(Some(_)) => state
            .store
            .mark_accepted(command.command_id.clone())
            .await
            .map_err(HubError::from),
        Ok(None) => {
            tracing::debug!(command_id = %command.command_id, "node offline at forward; not resent");
            state
                .store
                .get_command(command.command_id.clone())
                .await?
                .ok_or(HubError::NotFound)
        }
        Err(err) => {
            tracing::warn!(error = %err, command_id = %command.command_id, "node rpc unknown; will not resend");
            state
                .store
                .get_command(command.command_id.clone())
                .await?
                .ok_or(HubError::NotFound)
        }
    }
}

pub(crate) fn map_store(err: crate::store::StoreError) -> HubError {
    match &err {
        crate::store::StoreError::Id(msg) if msg.contains("unknown host") => HubError::NotFound,
        crate::store::StoreError::Id(msg)
            if msg.contains("reused") || msg.contains("idempotency") =>
        {
            HubError::Conflict(msg.clone())
        }
        crate::store::StoreError::Id(msg) => HubError::BadRequest(msg.clone()),
        other => HubError::Store(other.clone_as_internal()),
    }
}

impl crate::store::StoreError {
    fn clone_as_internal(&self) -> Self {
        crate::store::StoreError::Id(self.to_string())
    }
}
