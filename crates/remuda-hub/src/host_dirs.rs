//! Human-only host directory browser proxy (c-dirpicker).
//!
//! - `GET /v1/hosts/{id}/dirs[?path=…&showHidden=…]` → `host.dirs.list`
//!
//! Unlike the read-only host-file routes (Human **and** Bot operators), the
//! browser can enumerate directories that are not registered workspaces yet,
//! so it is restricted to Human-origin devices: a Bot token and an Agent
//! token both receive 403. The addressed host is always the path host; the
//! Node confines the listing to its configured `workspace_roots`.

use crate::agent_scope;
use crate::{AppState, HubError};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use remuda_protocol::InputOrigin;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/hosts/{id}/dirs", get(list_dirs))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirsQuery {
    /// Absolute Node path; absent lets the Node pick the default start.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    show_hidden: bool,
}

/// `GET /v1/hosts/{id}/dirs` — proxy a bounded directory-only listing.
async fn list_dirs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
    Query(query): Query<DirsQuery>,
) -> Result<Json<Value>, HubError> {
    crate::auth::require_origin(&headers, &state.config)?;
    let device = agent_scope::require_operator(&state, &headers).await?;
    if agent_scope::origin(&device) != InputOrigin::Human {
        // Directory browsing reveals the host filesystem beyond already
        // registered workspaces; only a seated human may use it.
        return Err(HubError::Forbidden);
    }
    if state.store.get_host(host_id.clone()).await?.is_none() {
        return Err(HubError::NotFound);
    }
    if state.nodes.kind_of(&host_id).await.is_none() {
        return Err(HubError::HostOffline { host_id });
    }
    // The path is forwarded verbatim: it is a filesystem-selected path, not
    // typed input, so it must never be trimmed (a trailing space is a legal
    // filename byte). An empty string means "default start"; any other value
    // (including whitespace) goes to the Node, which rejects it.
    let mut params = json!({"showHidden": query.show_hidden});
    if let Some(path) = query.path.as_deref().filter(|value| !value.is_empty()) {
        params["path"] = json!(path);
    }
    let body = crate::http::call_node(&state, &host_id, "host.dirs.list", params).await?;
    Ok(Json(body))
}
