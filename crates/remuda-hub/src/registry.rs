//! Host registry HTTP: get-by-id and operator PATCH (D-013).

use crate::AppState;
use crate::auth::{require_device, require_origin};
use crate::error::HubError;
use crate::inventory;
use crate::provider_resolve;
use crate::store::HostRecord;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};

/// Registry routes composed by the host feature router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/hosts/{id}", get(get_host).patch(patch_host))
        .route("/v1/hosts/{id}/doctor", get(doctor_host))
}

async fn doctor_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    if state.store.get_host(id.clone()).await?.is_none() {
        return Err(HubError::NotFound);
    }
    let report = crate::http::call_node(&state, &id, "host.doctor", json!({})).await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchHostBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    labels: Option<Value>,
    #[serde(default)]
    max_instances: Option<i64>,
    #[serde(default)]
    provider_binding: Option<String>,
    /// Per-host default extra CLI args. `null` clears the default.
    ///
    /// Double `Option` so "field absent" and "field set to null" stay
    /// distinguishable — without that there is no way to remove a default.
    #[serde(default, deserialize_with = "double_option")]
    default_launch_args: Option<Option<Vec<String>>>,
    /// Per-host default claude executable. `null` or `""` clears it.
    #[serde(default, deserialize_with = "double_option")]
    claude_binary_path: Option<Option<String>>,
    /// Per-host renderer preference. `null` clears it to fullscreen.
    #[serde(default, deserialize_with = "double_option")]
    default_tui: Option<Option<remuda_protocol::TuiMode>>,
    /// D-047 Amendment A1: explicit relay bind for direct-network routing.
    /// Double Option like the launch defaults — absent leaves it, `null`
    /// clears it back to loopback-only.
    #[serde(default, deserialize_with = "double_option")]
    relay_bind: Option<Option<remuda_protocol::HostRelayBind>>,
}

/// Deserialize a present-but-null field as `Some(None)`.
pub(crate) fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub(crate) fn host_view(host: &HostRecord) -> Value {
    json!({
        "hostId": host.host_id,
        "id": host.host_id,
        "name": host.label,
        "label": host.label,
        "state": host.state,
        "online": host.online,
        "lastSeenAt": host.last_seen_at,
        "nodeVersion": host.node_version,
        "cli": host.cli,
        "capabilities": host.capabilities,
        "instanceCount": host.instance_count,
        "transport": host.transport,
        "labels": host.labels,
        "herdr": host.herdr,
        "resources": host.resources,
        "maxInstances": host.max_instances,
        "hostname": host.hostname,
        "os": host.host_os,
        "ssh": host.ssh,
        "lastError": host.last_error,
        "providerBinding": host.provider_binding,
        "defaultLaunchArgs": host.default_launch_args,
        "claudeBinaryPath": host.claude_binary_path,
        "defaultTui": host.default_tui,
        "relayBind": host.relay_bind,
        "workspaces": host.workspaces,
        "workspaceRevision": host.workspace_revision,
    })
}

async fn get_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    let host = state
        .store
        .get_host(id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let live = state.nodes.kind_of(&host.host_id).await.is_some();
    Ok(Json(host_view(&crate::store::Store::with_live_link(
        host, live,
    ))))
}

async fn patch_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchHostBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    if state.store.get_host(id.clone()).await?.is_none() {
        return Err(HubError::NotFound);
    }
    let labels = body
        .labels
        .as_ref()
        .and_then(|value| inventory::from_node_params(&json!({ "labels": value })).labels);
    let provider_binding = match body.provider_binding.as_deref() {
        Some(raw) => Some(validate_binding(&state, &id, raw).await?),
        None => None,
    };
    // Validate the args default here so a bad flag is a 400 on the PATCH, not
    // a surprise the next time someone launches on this host. Same table the
    // Node uses; the Node still re-checks and stays the authority.
    if let Some(Some(args)) = body.default_launch_args.as_ref() {
        remuda_driver::validate_launch_args(remuda_protocol::DriverKind::ClaudePrint, args)
            .map_err(|error| HubError::BadRequest(error.to_string()))?;
    }
    // `claudeBinaryPath` gets no such check: the Hub cannot stat the Node's
    // filesystem, so anything it validated here would be a guess. It is stored
    // as given and the Node is the authority — a path that host rejects shows
    // up as a launch failure naming the reason.
    if let Some(Some(bind)) = body.relay_bind.as_ref() {
        provider_resolve::validate_relay_bind(bind).map_err(HubError::BadRequest)?;
    }
    let host = state
        .store
        .patch_host(
            id,
            body.name,
            labels,
            body.max_instances,
            provider_binding,
            crate::store::HostLaunchDefaultsPatch {
                default_launch_args: body.default_launch_args,
                claude_binary_path: body.claude_binary_path,
                default_tui: body.default_tui,
            },
            body.relay_bind,
        )
        .await
        .map_err(crate::http::map_store)?;
    let live = state.nodes.kind_of(&host.host_id).await.is_some();
    Ok(Json(host_view(&crate::store::Store::with_live_link(
        host, live,
    ))))
}

async fn validate_binding(state: &AppState, host_id: &str, raw: &str) -> Result<String, HubError> {
    let binding = provider_resolve::normalize_binding(raw).map_err(HubError::BadRequest)?;
    if let Some(profile_id) = binding.strip_prefix("profile:") {
        let profile = state
            .store
            .get_provider(profile_id.to_string())
            .await?
            .ok_or_else(|| {
                HubError::BadRequest(format!("unknown provider profile {profile_id}"))
            })?;
        if !provider_resolve::profile_allowed_on_host(&profile.scope, host_id) {
            return Err(HubError::BadRequest(format!(
                "profile {profile_id} is scoped to {}",
                profile.scope
            )));
        }
    }
    Ok(binding)
}
