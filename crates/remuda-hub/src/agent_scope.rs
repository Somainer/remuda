//! D-017: authenticated instance callers, immutable parent scope, and one-shot
//! human approvals. Grants disappear on Hub restart (fail closed).

use crate::{AppState, HubError, auth, store::Device};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use remuda_protocol::InputOrigin;
use serde_json::{Value, json};

/// Terminal aliases all resolve to a raw shell on the Node.
pub fn shell_driver(driver: &str) -> bool {
    matches!(driver, "shell-pty" | "shell" | "terminal")
}

pub fn origin(device: &Device) -> InputOrigin {
    if device.instance_id.is_some() {
        return InputOrigin::Agent;
    }
    match device.kind.as_str() {
        "human" => InputOrigin::Human,
        "bot" => InputOrigin::Bot,
        _ => InputOrigin::Agent,
    }
}

/// A caller header can narrow a Human/Bot token; it cannot promote a scoped token.
pub async fn caller(state: &AppState, headers: &HeaderMap) -> Result<Device, HubError> {
    let mut device = auth::require_device(&state.store, headers).await?;
    if let Some(raw) = headers.get("x-remuda-instance-id") {
        let id = raw.to_str().map_err(|_| HubError::Forbidden)?;
        if device
            .instance_id
            .as_deref()
            .is_some_and(|bound| bound != id)
        {
            return Err(HubError::Forbidden);
        }
        if state.store.get_instance(id.to_string()).await?.is_none() {
            return Err(HubError::Forbidden);
        }
        device.kind = "agent".into();
        device.instance_id = Some(id.into());
    }
    Ok(device)
}

pub fn stamp(payload: &mut Value, device: &Device) {
    payload["origin"] = json!(origin(device));
    if let Some(input) = payload.get_mut("input").and_then(Value::as_object_mut) {
        input.insert("origin".into(), json!(origin(device)));
    }
    // Never accept caller-selected provenance or a credential in JSON.
    if let Some(object) = payload.as_object_mut() {
        object.remove("agentCredential");
        object.remove("actor");
    }
}

pub async fn owns(state: &AppState, device: &Device, target: &str) -> Result<bool, HubError> {
    let Some(caller) = device.instance_id.as_deref() else {
        return Ok(false);
    };
    let instance = state
        .store
        .get_instance(target.into())
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(caller == target || instance.parent_instance_id.as_deref() == Some(caller))
}

pub async fn same_host(state: &AppState, device: &Device, host: &str) -> Result<bool, HubError> {
    let Some(caller) = device.instance_id.as_deref() else {
        return Ok(false);
    };
    let instance = state
        .store
        .get_instance(caller.into())
        .await?
        .ok_or(HubError::Forbidden)?;
    Ok(instance.host_id == host)
}

pub async fn prepare_create(
    state: &AppState,
    headers: &HeaderMap,
    device: &Device,
    host: &str,
    driver: &str,
    spec: &mut Value,
) -> Result<(), HubError> {
    stamp(spec, device);
    spec["parentInstanceId"] = json!(device.instance_id);
    if origin(device) != remuda_protocol::InputOrigin::Human && spec["permissionMode"].is_null() {
        spec["permissionMode"] = json!("manual");
    }
    if origin(device) == remuda_protocol::InputOrigin::Agent
        && (!same_host(state, device, host).await?
            || shell_driver(driver)
            || headers.contains_key("x-remuda-require-approval"))
    {
        require_approval(
            state,
            headers,
            device,
            json!({"operation":"instance.create", "hostId":host, "spec":spec}),
        )
        .await?;
    }
    Ok(())
}

pub async fn authorize_command(
    state: &AppState,
    headers: &HeaderMap,
    device: &Device,
    instance: &crate::store::InstanceRecord,
    operation: &str,
    payload: &mut Value,
) -> Result<(), HubError> {
    stamp(payload, device);
    if origin(device) == remuda_protocol::InputOrigin::Agent {
        if !matches!(
            operation,
            "instance.send" | "instance.cancel" | "instance.close" | "tty.write" | "instance.keys"
        ) {
            return Err(HubError::Forbidden);
        }
        if matches!(operation, "tty.write" | "instance.keys")
            || (operation == "instance.send" && shell_driver(&instance.driver))
            || !owns(state, device, &instance.instance_id).await?
            || headers.contains_key("x-remuda-require-approval")
        {
            require_approval(
                state,
                headers,
                device,
                json!({"operation":operation, "instanceId":instance.instance_id, "payload":payload}),
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn require_approval(
    state: &AppState,
    headers: &HeaderMap,
    device: &Device,
    request: Value,
) -> Result<(), HubError> {
    let id = device.instance_id.clone().ok_or(HubError::Forbidden)?;
    let instance = state
        .store
        .get_instance(id.clone())
        .await?
        .ok_or(HubError::Forbidden)?;
    let caller = crate::agent_approvals::ApprovalCaller {
        device_id: device.id.clone(),
        instance_id: id.parse().map_err(|_| HubError::Forbidden)?,
        host_id: instance.host_id.parse().map_err(|_| HubError::Forbidden)?,
    };
    state
        .agent_approvals
        .require(headers, &caller, request)
        .await
}

pub fn check_fleet_all(device: &Device, all: bool, confirm: bool) -> Result<(), HubError> {
    if all && origin(device) == InputOrigin::Agent {
        return Err(HubError::BadRequest(
            "fleet all is forbidden from Agent origin".into(),
        ));
    }
    if all && !confirm {
        return Err(HubError::BadRequest(
            "fleet all requires explicit confirm=true".into(),
        ));
    }
    Ok(())
}

pub async fn check_broadcast(
    state: &AppState,
    headers: &HeaderMap,
    device: &Device,
    targets: &[&crate::store::InstanceRecord],
    operation: &str,
    payload: &Value,
) -> Result<(), HubError> {
    if origin(device) != InputOrigin::Agent || targets.is_empty() {
        return Ok(());
    }
    let mut approval =
        operation == "tty.write" || headers.contains_key("x-remuda-require-approval");
    for target in targets {
        approval |=
            shell_driver(&target.driver) || !owns(state, device, &target.instance_id).await?;
    }
    if approval {
        require_approval(state, headers, device, json!({"operation":"fleet.broadcast", "command":operation,
            "targets":targets.iter().map(|row| &row.instance_id).collect::<Vec<_>>(), "payload":payload})).await?;
    }
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/caller", get(get_caller))
        .route("/v1/instances/{id}/mcp-token", post(mint_token))
}

async fn get_caller(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    let instance = match &device.instance_id {
        Some(id) => state.store.get_instance(id.clone()).await?,
        None => None,
    };
    let children: Vec<String> = state
        .store
        .list_instances(None)
        .await?
        .into_iter()
        .filter(|row| {
            device
                .instance_id
                .as_deref()
                .is_some_and(|id| row.parent_instance_id.as_deref() == Some(id))
        })
        .map(|row| row.instance_id)
        .collect();
    Ok(Json(
        json!({"origin": origin(&device), "instanceId": device.instance_id,
        "hostId": instance.map(|row| row.host_id), "children": children}),
    ))
}

async fn mint_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    auth::require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    if origin(&device) != InputOrigin::Human {
        return Err(HubError::Forbidden);
    }
    state
        .store
        .get_instance(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(Json(
        json!({"token": instance_token(state.store.clone(), id).await?, "origin": "agent"}),
    ))
}

impl crate::RunningHub {
    /// Mint a Bot device credential for an in-process dispatcher (D-017/D-018).
    pub async fn mint_bot_device_token(&self, device_name: &str) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = crate::config::random_token();
        let hash = auth::hash_secret(&token)?;
        let prefix = auth::token_prefix(&token)
            .ok_or_else(|| anyhow::anyhow!("generated device token is not indexable"))?
            .to_owned();
        store
            .insert_device_as(device_name.into(), hash, prefix, "bot".into(), None)
            .await?;
        Ok(token)
    }
}

pub async fn instance_token(
    store: crate::store::Store,
    instance_id: String,
) -> Result<String, HubError> {
    let token = crate::config::random_token();
    let hash = auth::hash_secret(&token)?;
    store
        .insert_device_as(
            format!("instance:{instance_id}"),
            hash,
            token[..16].to_owned(),
            "agent".into(),
            Some(instance_id),
        )
        .await?;
    Ok(token)
}

/// Scoped credentials cannot mint operator credentials, answer their own
/// approvals, mutate host/worktree administration, or use the raw tty socket.
pub async fn restrict_agent_routes(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, HubError> {
    let path = request.uri().path();
    if !matches!(
        path,
        "/v1/node" | "/node/v1/connect" | "/v1/login" | "/v1/devices/pair" | "/healthz"
    ) && auth::presented_token(request.headers()).is_some()
    {
        let device = caller(&state, request.headers()).await?;
        if origin(&device) == InputOrigin::Agent {
            let read = request.method() == axum::http::Method::GET && path != "/v1/follow";
            let write = request.method() == axum::http::Method::POST
                && (path == "/v1/instances"
                    || path == "/v1/fleet/instances"
                    || path == "/v1/fleet/broadcast"
                    || (path.starts_with("/v1/instances/") && path.ends_with("/commands")));
            if !read && !write {
                return Err(HubError::Forbidden);
            }
        }
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_kind_and_instance_binding_determine_origin() {
        let mut device = Device {
            id: "device".into(),
            name: "human".into(),
            kind: "unknown".into(),
            instance_id: None,
        };
        assert_eq!(origin(&device), InputOrigin::Agent);
        device.kind = "bot".into();
        assert_eq!(origin(&device), InputOrigin::Bot);
        device.kind = "human".into();
        assert_eq!(origin(&device), InputOrigin::Human);
        device.instance_id = Some("instance".into());
        assert_eq!(origin(&device), InputOrigin::Agent);
        let mut payload = json!({"origin":"human", "actor":{"type":"human"}, "input":{"origin":"human"}, "agentCredential":{"token":"forged"}});
        stamp(&mut payload, &device);
        assert_eq!(payload["origin"], "agent");
        assert_eq!(payload["input"]["origin"], "agent");
        assert!(payload.get("actor").is_none());
        assert!(payload.get("agentCredential").is_none());
    }
}
