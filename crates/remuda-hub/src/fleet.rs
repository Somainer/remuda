//! Fan-out the same Instance spec across N hosts (D-013).

use crate::AppState;
use crate::auth::{require_device, require_origin};
use crate::error::HubError;
use crate::placement::{self, PlaceSpec, Placement};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

/// Fleet HTTP surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/fleet/instances", post(create_fleet))
        .route("/v1/fleet/{id}", get(get_fleet))
        .route("/v1/fleet/{id}/commands", post(fleet_commands))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateFleetBody {
    #[serde(default)]
    spec: Value,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    max: Option<usize>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FleetCommandBody {
    #[serde(default = "default_op")]
    operation: String,
    #[serde(default)]
    payload: Value,
}

fn default_op() -> String {
    "instance.send".into()
}

async fn create_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateFleetBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let mut spec = body.spec;
    if spec.is_null() {
        spec = json!({});
    }
    if let Some(obj) = spec.as_object_mut() {
        if let Some(kind) = &body.kind {
            obj.insert("kind".into(), json!(kind));
        }
        if let Some(driver) = &body.driver {
            obj.insert("driver".into(), json!(driver));
        }
        if let Some(title) = &body.title {
            obj.insert("title".into(), json!(title));
        }
        if let Some(prompt) = &body.prompt {
            obj.insert("prompt".into(), json!(prompt));
        }
    }
    let kind = spec
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("claude")
        .to_string();
    let driver = spec
        .get("driver")
        .and_then(Value::as_str)
        .unwrap_or("claude-print")
        .to_string();
    let place_spec = PlaceSpec::from_json(&spec);
    let mut chosen = Vec::new();
    if !body.hosts.is_empty() {
        for host_id in &body.hosts {
            let hosts = placement::pick_hosts(
                &state,
                &Placement::Host {
                    host_id: host_id.clone(),
                },
                &place_spec,
            )
            .await?;
            chosen.extend(hosts);
        }
    } else if !body.labels.is_empty() {
        chosen = placement::pick_hosts(
            &state,
            &Placement::Labels {
                labels: body.labels.clone(),
            },
            &place_spec,
        )
        .await?;
    } else {
        chosen = placement::pick_hosts(&state, &Placement::Any, &place_spec).await?;
    }
    if let Some(max) = body.max {
        chosen.truncate(max.max(1));
    }
    if chosen.is_empty() {
        return Err(HubError::Unsatisfiable {
            reasons: vec!["fleet matched no hosts".into()],
        });
    }
    let mut members = Vec::new();
    let mut instance_ids = Vec::new();
    for host in &chosen {
        let mut host_spec = spec.clone();
        if let Some(obj) = host_spec.as_object_mut() {
            obj.insert("hostId".into(), json!(host.host_id));
        }
        let (instance, _command) = placement::spawn_on_host(
            &state,
            host,
            placement::SpawnRequest {
                kind: kind.clone(),
                driver: driver.clone(),
                workspace_id: body.workspace_id.clone(),
                title: body.title.clone(),
                prompt: body.prompt.clone(),
                spec: host_spec,
            },
        )
        .await?;
        instance_ids.push(instance.instance_id.clone());
        members.push((instance.instance_id, host.host_id.clone()));
    }
    let fleet_id = state.store.insert_fleet(spec, members).await?;
    Ok(Json(json!({
        "fleetId": fleet_id,
        "instanceIds": instance_ids,
    })))
}

async fn get_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    let Some((spec, members)) = state.store.get_fleet(id.clone()).await? else {
        return Err(HubError::NotFound);
    };
    let mut instances = Vec::new();
    for (instance_id, host_id) in &members {
        let instance = state.store.get_instance(instance_id.clone()).await?;
        instances.push(json!({
            "instanceId": instance_id,
            "hostId": host_id,
            "lifecycle": instance.as_ref().map(|i| i.lifecycle.clone()),
            "activity": instance.as_ref().map(|i| i.activity.clone()),
            "connectivity": instance.as_ref().map(|i| i.connectivity.clone()),
        }));
    }
    Ok(Json(json!({
        "fleetId": id,
        "spec": spec,
        "instanceIds": members.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        "instances": instances,
    })))
}

async fn fleet_commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<FleetCommandBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let Some((_spec, members)) = state.store.get_fleet(id.clone()).await? else {
        return Err(HubError::NotFound);
    };
    let mut commands = Vec::new();
    for (instance_id, host_id) in members {
        let mut payload = body.payload.clone();
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
                None,
                Some(instance_id.clone()),
                host_id.clone(),
                body.operation.clone(),
                payload,
                None,
            )
            .await
            .map_err(crate::http::map_store)?;
        let online = state
            .store
            .get_host(host_id)
            .await?
            .map(|h| h.online)
            .unwrap_or(false);
        let command = if created {
            crate::http::forward_if_online(&state, command, online).await?
        } else {
            command
        };
        commands.push(json!({
            "instanceId": instance_id,
            "commandId": command.command_id,
            "state": command.state,
            "resolution": command.resolution,
            "forwarded": command.forwarded,
        }));
    }
    Ok(Json(json!({
        "fleetId": id,
        "commands": commands,
    })))
}
