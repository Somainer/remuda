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
        .route("/v1/fleet/broadcast", post(broadcast))
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

/// `POST /v1/fleet/broadcast` body: select running instances, fan one command out.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BroadcastBody {
    /// Every running instance. Required unless `hosts` / `labels` / `kinds`
    /// narrow the set.
    #[serde(default)]
    all: bool,
    /// Explicit operator confirmation for all-target broadcasts.
    #[serde(default)]
    confirm: bool,
    /// Restrict to these host ids.
    #[serde(default)]
    hosts: Vec<String>,
    /// Restrict to hosts carrying every one of these `key=value` labels.
    #[serde(default)]
    labels: Vec<String>,
    /// Restrict to these agent kinds (`claude`, `codex`, …).
    #[serde(default)]
    kinds: Vec<String>,
    /// `instance.send` (default) or `tty.write`.
    #[serde(default = "default_op")]
    operation: String,
    /// Per-instance command payload; `instanceId` is filled in per member.
    #[serde(default)]
    payload: Value,
    /// Base key; each instance gets `<key>:<instanceId>` so a retried
    /// broadcast replays instead of double-sending.
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// Instances a broadcast may target. Terminal and draining lifecycles are
/// skipped; `requested` / `starting` are kept because the Hub queues their
/// commands until the pane is live.
fn is_broadcast_target(lifecycle: &str) -> bool {
    !matches!(lifecycle, "exited" | "failed" | "closing")
}

/// `POST /v1/fleet/broadcast` — fan `operation` out to every matching running
/// instance. Selection is `all` OR any of `hosts` / `labels` / `kinds`; the
/// filters intersect. Per-instance results carry their own error so one
/// offline host cannot fail the batch.
async fn broadcast(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<BroadcastBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    crate::agent_scope::check_fleet_all(&device, body.all, body.confirm)?;
    if body.payload.is_null() {
        body.payload = json!({});
    }
    if !body.payload.is_object() {
        return Err(HubError::BadRequest(
            "broadcast payload must be an object".into(),
        ));
    }
    crate::agent_scope::stamp(&mut body.payload, &device);
    let filtered = !body.hosts.is_empty() || !body.labels.is_empty() || !body.kinds.is_empty();
    if !body.all && !filtered {
        return Err(HubError::BadRequest(
            "broadcast requires all=true or one of hosts/labels/kinds".into(),
        ));
    }
    if !matches!(body.operation.as_str(), "instance.send" | "tty.write") {
        return Err(HubError::BadRequest(format!(
            "broadcast operation must be instance.send or tty.write, got {}",
            body.operation
        )));
    }

    let hosts = state.store.list_hosts().await?;
    let mut allowed: Option<Vec<String>> = None;
    if !body.hosts.is_empty() {
        allowed = Some(body.hosts.clone());
    }
    if !body.labels.is_empty() {
        let matched: Vec<String> = hosts
            .iter()
            .filter(|host| {
                body.labels
                    .iter()
                    .all(|label| placement::host_has_label(host, label))
            })
            .map(|host| host.host_id.clone())
            .collect();
        allowed = Some(match allowed {
            Some(prev) => prev.into_iter().filter(|id| matched.contains(id)).collect(),
            None => matched,
        });
    }

    let instances = state.store.list_instances(None).await?;
    let targets: Vec<_> = instances
        .iter()
        .filter(|row| {
            is_broadcast_target(&row.lifecycle)
                && allowed
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&row.host_id))
                && (body.kinds.is_empty() || body.kinds.contains(&row.kind))
        })
        .collect();
    crate::agent_scope::check_broadcast(
        &state,
        &headers,
        &device,
        &targets,
        &body.operation,
        &body.payload,
    )
    .await?;

    let mut results = Vec::new();
    let (mut accepted, mut failed, mut skipped) = (0usize, 0usize, 0usize);
    for instance in instances {
        if !is_broadcast_target(&instance.lifecycle) {
            skipped += 1;
            continue;
        }
        if let Some(ids) = &allowed
            && !ids.contains(&instance.host_id)
        {
            skipped += 1;
            continue;
        }
        if !body.kinds.is_empty() && !body.kinds.iter().any(|k| k == &instance.kind) {
            skipped += 1;
            continue;
        }
        let mut payload = body.payload.clone();
        if payload.is_null() {
            payload = json!({});
        }
        let Some(obj) = payload.as_object_mut() else {
            return Err(HubError::BadRequest(
                "broadcast payload must be an object".into(),
            ));
        };
        obj.insert("instanceId".into(), json!(instance.instance_id.clone()));
        // Per-instance derivation: one key per (broadcast, instance) pair.
        let key = body
            .idempotency_key
            .as_ref()
            .map(|base| format!("{base}:{}", instance.instance_id));
        let queued = state
            .store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                instance.host_id.clone(),
                body.operation.clone(),
                payload,
                key,
            )
            .await
            .map_err(crate::http::map_store);
        let (command, created) = match queued {
            Ok(pair) => pair,
            Err(err) => {
                failed += 1;
                results.push(json!({
                    "instanceId": instance.instance_id,
                    "hostId": instance.host_id,
                    "kind": instance.kind,
                    "ok": false,
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let online = hosts
            .iter()
            .find(|host| host.host_id == instance.host_id)
            .map(|host| host.online)
            .unwrap_or(false);
        let command = if created {
            match crate::http::forward_if_online(&state, command.clone(), online).await {
                Ok(forwarded) => forwarded,
                Err(err) => {
                    failed += 1;
                    results.push(json!({
                        "instanceId": instance.instance_id,
                        "hostId": instance.host_id,
                        "kind": instance.kind,
                        "ok": false,
                        "commandId": command.command_id,
                        "error": err.to_string(),
                    }));
                    continue;
                }
            }
        } else {
            command
        };
        accepted += 1;
        results.push(json!({
            "instanceId": instance.instance_id,
            "hostId": instance.host_id,
            "kind": instance.kind,
            "ok": true,
            "commandId": command.command_id,
            "state": command.state,
            "resolution": command.resolution,
            "forwarded": command.forwarded,
            "replayed": !created,
        }));
    }
    Ok(Json(json!({
        "operation": body.operation,
        "accepted": accepted,
        "failed": failed,
        "skipped": skipped,
        "selected": results.len(),
        "results": results,
    })))
}

async fn create_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateFleetBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
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
    crate::agent_scope::stamp(&mut spec, &device);
    spec["parentInstanceId"] = json!(device.instance_id);
    if crate::agent_scope::origin(&device) != remuda_protocol::InputOrigin::Human
        && spec["permissionMode"].is_null()
    {
        spec["permissionMode"] = json!("manual");
    }
    if crate::agent_scope::origin(&device) == remuda_protocol::InputOrigin::Agent {
        let mut approval_required = headers.contains_key("x-remuda-require-approval")
            || crate::agent_scope::shell_driver(&driver);
        for host in &chosen {
            approval_required |=
                !crate::agent_scope::same_host(&state, &device, &host.host_id).await?;
        }
        if approval_required {
            crate::agent_scope::require_approval(&state, &headers, &device, json!({"operation":"fleet.create", "hosts":chosen.iter().map(|h| &h.host_id).collect::<Vec<_>>(), "spec":spec})).await?;
        }
    }
    let mut members = Vec::new();
    let mut instance_ids = Vec::new();
    for host in &chosen {
        let mut host_spec = spec.clone();
        if let Some(obj) = host_spec.as_object_mut() {
            obj.insert("hostId".into(), json!(host.host_id));
        }
        crate::providers::resolve_and_attach(&state, host, &mut host_spec).await?;
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
    let device = crate::agent_scope::caller(&state, &headers).await?;
    if crate::agent_scope::origin(&device) == remuda_protocol::InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
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
        crate::agent_scope::stamp(&mut payload, &device);
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
        let live = state.nodes.kind_of(&host_id).await.is_some();
        let command = if created {
            crate::http::forward_if_online(&state, command, live).await?
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
