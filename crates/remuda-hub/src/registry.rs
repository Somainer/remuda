//! Host registry HTTP: get-by-id and operator PATCH (D-013).

use crate::AppState;
use crate::auth::{require_device, require_origin};
use crate::error::HubError;
use crate::inventory;
use crate::store::HostRecord;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};

/// Host registry routes. `GET /v1/hosts` stays on `http::routes`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/hosts/{id}", get(get_host).patch(patch_host))
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
    })
}

async fn get_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    let host = state.store.get_host(id).await?.ok_or(HubError::NotFound)?;
    Ok(Json(host_view(&host)))
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
    let host = state
        .store
        .patch_host(id, body.name, labels, body.max_instances)
        .await
        .map_err(crate::http::map_store)?;
    Ok(Json(host_view(&host)))
}
