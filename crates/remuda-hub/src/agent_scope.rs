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
    let Some(instance) = state.store.get_instance(target.into()).await? else {
        return Ok(false);
    };
    Ok(caller == target || instance.parent_instance_id.as_deref() == Some(caller))
}

/// Fleet-wide metadata is available only to operator devices.
pub async fn require_operator(state: &AppState, headers: &HeaderMap) -> Result<Device, HubError> {
    let device = caller(state, headers).await?;
    if origin(&device) == InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
    Ok(device)
}

/// Load the caller's delegation scope; design §2.5.
///
/// Human/Bot devices are the universe root — their seat may touch any
/// resource, with route-level operator checks deciding whether they may.
/// Agent credentials inherit the scope stored on the bound instance.
pub async fn caller_project_scope(
    state: &AppState,
    device: &Device,
) -> Result<remuda_protocol::InstanceScope, HubError> {
    let Some(id) = device.instance_id.as_deref() else {
        return Ok(remuda_protocol::InstanceScope::universe());
    };
    let instance = state
        .store
        .get_instance(id.into())
        .await?
        .ok_or(HubError::Forbidden)?;
    Ok(instance.scope)
}

/// An Agent caller must hold `grant`; design §2.5 (enforcement reads grants,
/// never the display-only `role`).
pub async fn require_grant(
    state: &AppState,
    device: &Device,
    grant: remuda_protocol::GrantVerb,
) -> Result<(), HubError> {
    if origin(device) != InputOrigin::Agent {
        return Ok(());
    }
    let Some(id) = device.instance_id.as_deref() else {
        return Err(HubError::Forbidden);
    };
    let instance = state
        .store
        .get_instance(id.into())
        .await?
        .ok_or(HubError::Forbidden)?;
    let wire = serde_json::to_value(grant)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    if instance.grants.iter().any(|held| held == &wire) {
        Ok(())
    } else {
        Err(HubError::Forbidden)
    }
}

/// Repeat the middleware ownership check at the instance read boundary.
pub async fn require_instance_read(
    state: &AppState,
    headers: &HeaderMap,
    instance_id: &str,
) -> Result<(), HubError> {
    let device = caller(state, headers).await?;
    if origin(&device) == InputOrigin::Agent && !owns(state, &device, instance_id).await? {
        return Err(HubError::Forbidden);
    }
    Ok(())
}

fn read_target(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1/instances/")?;
    let id = rest.strip_suffix("/journal").unwrap_or(rest);
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// The two D-028 attachment reads, which authorize themselves against the
/// caller's own session id rather than a path segment.
fn attachment_read(path: &str) -> bool {
    path == "/v1/attachments"
        || path
            .strip_prefix("/v1/attachments/")
            .and_then(|rest| rest.strip_suffix("/content"))
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// `GET /v1/projects` or `GET /v1/projects/{id}`; design §2.4.
///
/// The handler still filters by the caller's scope and requires the
/// `dispatch` grant for Agent callers (leaf workers stay at 403).
fn project_read_target(path: &str) -> bool {
    if path == "/v1/projects" {
        return true;
    }
    match path.strip_prefix("/v1/projects/") {
        Some(rest) => !rest.is_empty() && !rest.contains('/'),
        None => false,
    }
}

/// `PATCH /v1/projects/{id}` or `POST|DELETE /v1/projects/{id}/members`.
fn project_write_target(path: &str) -> bool {
    match path.strip_prefix("/v1/projects/") {
        Some(rest) => {
            if let Some(rest) = rest.strip_suffix("/members") {
                return !rest.is_empty() && !rest.contains('/');
            }
            !rest.is_empty() && !rest.contains('/')
        }
        None => false,
    }
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

/// Agent launches retain only manual or plan permissions on every create path.
pub fn restrict_permission(device: &Device, spec: &mut Value) -> Result<(), HubError> {
    if origin(device) != remuda_protocol::InputOrigin::Human && spec["permissionMode"].is_null() {
        spec["permissionMode"] = json!("manual");
    }
    if origin(device) == InputOrigin::Agent
        && !matches!(spec["permissionMode"].as_str(), Some("manual" | "plan"))
    {
        return Err(HubError::Forbidden);
    }
    Ok(())
}

/// Agent callers may not choose their own argv or executable.
///
/// The spec fields are operator-level: `args` picks flags that the allowlist
/// would otherwise have to defend alone, and `binaryPath` picks the code that
/// runs. An instance never inherits operator authority, so both are refused
/// outright rather than sanitized. The Node refuses them again during
/// materialization; this is the early, cheap half.
pub fn restrict_launch_overrides(device: &Device, spec: &Value) -> Result<(), HubError> {
    if origin(device) != InputOrigin::Agent {
        return Ok(());
    }
    let has_args = spec["args"].as_array().is_some_and(|args| !args.is_empty());
    let has_binary = spec["binaryPath"]
        .as_str()
        .is_some_and(|value| !value.trim().is_empty());
    if has_args || has_binary {
        return Err(HubError::Forbidden);
    }
    Ok(())
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
    restrict_permission(device, spec)?;
    restrict_launch_overrides(device, spec)?;
    if origin(device) == remuda_protocol::InputOrigin::Agent {
        // §2.4/§2.5: the chosen host and workspace must be inside the
        // caller's scope, and the caller needs dispatch to delegate at all.
        // The store re-checks scope subset/grants against this same parent
        // row inside the writer; this is the early, cheap rejection.
        require_grant(state, device, remuda_protocol::GrantVerb::Dispatch).await?;
        let caller_id = device.instance_id.as_deref().ok_or(HubError::Forbidden)?;
        let caller_instance = state
            .store
            .get_instance(caller_id.into())
            .await?
            .ok_or(HubError::Forbidden)?;
        if !caller_instance.scope.allows_host(host) {
            return Err(HubError::Forbidden);
        }
        if let Some(workspace) = spec
            .get("workspaceId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            && !caller_instance.scope.allows_workspace(workspace)
        {
            return Err(HubError::Forbidden);
        }
        if !same_host(state, device, host).await?
            || shell_driver(driver)
            || headers.contains_key("x-remuda-require-approval")
        {
            require_approval(
                state,
                headers,
                device,
                json!({"operation":"instance.create", "hostId":host, "spec":spec}),
            )
            .await?;
        }
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
        "hostId": instance.as_ref().map(|row| row.host_id.clone()),
        "role": instance.as_ref().and_then(|row| row.role.clone()),
        "projectId": instance.as_ref().and_then(|row| row.project_id.clone()),
        "scope": instance.as_ref().map(|row| row.scope.clone())
            .unwrap_or_else(remuda_protocol::InstanceScope::universe),
        "grants": instance.as_ref().map(|row| row.grants.clone()).unwrap_or_default(),
        "children": children}),
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
    let prefix = auth::token_prefix(&token)
        .ok_or_else(|| HubError::Internal("generated device token is not indexable".into()))?
        .to_owned();
    store
        .insert_device_as(
            format!("instance:{instance_id}"),
            hash,
            prefix,
            "agent".into(),
            Some(instance_id),
        )
        .await?;
    Ok(token)
}

/// Routes that establish, rather than require, a device session. They must run
/// even when the request carries a stale or otherwise-invalid presented token:
/// the login credential (access code, pair code, or a WebAuthn assertion) is in
/// the body, not the bearer/cookie. Passkey register routes are deliberately
/// absent — they require an existing device session.
fn is_auth_establishing(path: &str) -> bool {
    matches!(
        path,
        "/v1/node"
            | "/node/v1/connect"
            | "/v1/login"
            | "/v1/devices/pair"
            | "/v1/auth/passkeys/login/start"
            | "/v1/auth/passkeys/login/finish"
            | "/healthz"
    )
}

/// Scoped credentials cannot mint operator credentials, answer their own
/// approvals, mutate host/worktree administration, or use the raw tty socket.
pub async fn restrict_agent_routes(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, HubError> {
    let path = request.uri().path();
    // D-027: a Node pulls `GET /v1/objects/{id}` with its *host* token, which
    // is not a device and would fail `caller` here with 401 before the handler
    // ever runs. The handler authorizes that route itself, and its device
    // branch is strictly stronger than this one (operator only, so Agent
    // origin is refused there too), so skipping it loses no check.
    let node_object_read =
        request.method() == axum::http::Method::GET && path.starts_with("/v1/objects/");
    if !node_object_read
        && !is_auth_establishing(path)
        && auth::presented_token(request.headers()).is_some()
    {
        let device = caller(&state, request.headers()).await?;
        if origin(&device) == InputOrigin::Agent {
            let read = request.method() == axum::http::Method::GET
                && (path == "/v1/caller"
                    // D-028 §4.5: the in-session MCP server reads its own
                    // session's attachments. The handlers pin every read to
                    // the credential's own instance — strictly narrower than
                    // `owns`, which also admits direct children.
                    || attachment_read(path)
                    // §2.4: coordinators list/read projects and instances
                    // inside their scope; handlers re-check grants + scope.
                    || project_read_target(path)
                    || path == "/v1/instances"
                    || match read_target(path) {
                        Some(id) => owns(&state, &device, id).await?,
                        None => false,
                    });
            let write = request.method() == axum::http::Method::POST
                && (path == "/v1/instances"
                    || path == "/v1/fleet/instances"
                    || path == "/v1/fleet/broadcast"
                    || (path.starts_with("/v1/instances/") && path.ends_with("/commands"))
                    || (path.starts_with("/v1/projects/") && path.ends_with("/members")))
                || request.method() == axum::http::Method::PATCH && project_write_target(path)
                || (request.method() == axum::http::Method::DELETE
                    && path
                        .strip_prefix("/v1/projects/")
                        .is_some_and(|rest| rest.ends_with("/members")));
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
    fn passkey_login_routes_are_auth_establishing_but_register_is_not() {
        // A stale post-logout cookie on the immediately-fired conditional
        // ceremony must not be resolved by `caller` (which 401s) before the
        // WebAuthn handler runs. Both login routes sit beside /v1/login.
        assert!(is_auth_establishing("/v1/auth/passkeys/login/start"));
        assert!(is_auth_establishing("/v1/auth/passkeys/login/finish"));
        assert!(is_auth_establishing("/v1/login"));
        // Registering still requires a live device session.
        assert!(!is_auth_establishing("/v1/auth/passkeys/register/start"));
        assert!(!is_auth_establishing("/v1/auth/passkeys/register/finish"));
        assert!(!is_auth_establishing("/v1/auth/passkeys"));
        // Listing/management are not auth-establishing either.
        assert!(!is_auth_establishing("/v1/devices"));
    }

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

    /// The middleware only lets the two D-028 shapes through; anything else
    /// under `/v1/attachments` stays behind the Agent-origin refusal.
    #[test]
    fn only_the_two_attachment_read_shapes_bypass_the_instance_path_check() {
        for path in ["/v1/attachments", "/v1/attachments/obj_1/content"] {
            assert!(attachment_read(path), "{path} must be readable");
        }
        for path in [
            "/v1/attachments/",
            "/v1/attachments/obj_1",
            "/v1/attachments/obj_1/content/extra",
            "/v1/attachments/obj_1/bytes",
            "/v1/attachmentsx",
        ] {
            assert!(!attachment_read(path), "{path} must not be readable");
        }
    }
}
