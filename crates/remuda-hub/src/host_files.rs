//! Read-only host file listing and fetch (the upward counterpart of D-027).
//!
//! Two operator routes proxy to the addressed Node:
//! - `GET  /v1/hosts/{id}/files?workspaceId=…[&relPath=…]` → `host.files.list`
//! - `POST /v1/hosts/{id}/files/read` `{workspaceId, relPath}` → `host.files.read`
//!
//! Both require an operator device; Agent origin is refused with 403 here and
//! is absent from the agent-scope middleware allowlist. The addressed host is
//! always the path host: the proxy never lets one host answer for another.
//!
//! `POST /v1/hosts/{id}/files/objects` is the Node-facing staging endpoint the
//! Node's `host.files.read` uploads bytes to with its durable host token. The
//! bytes land in the same objects store and flow back out through the existing
//! `GET /v1/objects/{id}`.

use crate::AppState;
use crate::HubError;
use crate::auth::require_origin;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use remuda_protocol::hubnode::sanitize_attachment_name;
use serde::Deserialize;
use serde_json::{Value, json};

/// Same one-MiB slack the attachment uploader allows into the buffer layer.
const BODY_LIMIT_SLACK: usize = 1024 * 1024;
/// Per-host staging area budget: ten 25 MiB files.
const HOST_FILES_BUDGET: i64 = 256 * 1024 * 1024;
/// Staging lifetime, matching attachment objects.
const HOST_FILES_TTL_SECONDS: i64 = 24 * 60 * 60;

pub fn routes(max_object_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/v1/hosts/{id}/files", get(list_files))
        .route("/v1/hosts/{id}/files/read", post(read_file))
        .route("/v1/hosts/{id}/files/objects", post(stage_object))
        .layer(axum::middleware::from_fn_with_state(
            max_object_bytes,
            reject_oversize_stage,
        ))
        .layer(DefaultBodyLimit::max(max_object_bytes + BODY_LIMIT_SLACK))
}

/// Objects-table staging key for one host's read-only fetches. It is never a
/// real `ins_…` row, so host-file objects never mix into an instance's budget
/// or get swept with an exited instance.
fn staging_instance(host_id: &str) -> String {
    format!("host-files:{host_id}")
}

/// 413 on a declared Content-Length already past the cap, before the body is
/// streamed. Same shape as the attachment uploader's preflight.
async fn reject_oversize_stage(
    State(max_bytes): State<usize>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    if method == Method::POST
        && uri.path().ends_with("/files/objects")
        && let Some(length) = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<usize>().ok())
        && length > max_bytes
    {
        let body = json!({
            "error": format!(
                "RESOURCE_LIMIT: host file is {length} bytes; the limit is {max_bytes}"
            ),
            "code": "RESOURCE_LIMIT",
        });
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(header::CONTENT_TYPE, "application/json")],
            Json(body),
        )
            .into_response();
    }
    next.run(request).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    workspace_id: String,
    #[serde(default)]
    rel_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadBody {
    workspace_id: String,
    #[serde(default)]
    rel_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StageQuery {
    /// Basename of the file as read on the Node; sanitised server-side.
    #[serde(default)]
    name: Option<String>,
}

/// `GET /v1/hosts/{id}/files` — proxy a directory listing to the Node.
async fn list_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let node_params = node_params(&query.workspace_id, query.rel_path.as_deref())?;
    proxy_to_host(&state, &headers, &host_id, "host.files.list", node_params).await
}

/// `POST /v1/hosts/{id}/files/read` — proxy a file read; the Node stages the
/// bytes and replies with the object id.
async fn read_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
    Json(body): Json<ReadBody>,
) -> Result<Json<Value>, HubError> {
    let node_params = node_params(&body.workspace_id, body.rel_path.as_deref())?;
    proxy_to_host(&state, &headers, &host_id, "host.files.read", node_params).await
}

fn node_params(workspace_id: &str, rel_path: Option<&str>) -> Result<Value, HubError> {
    let workspace_id = workspace_id.trim();
    if workspace_id.is_empty() {
        return Err(HubError::BadRequest("workspaceId is required".into()));
    }
    let mut params = json!({"workspaceId": workspace_id});
    if let Some(rel_path) = rel_path.map(str::trim).filter(|value| !value.is_empty()) {
        params["relPath"] = json!(rel_path);
    }
    Ok(params)
}

/// Operator gate, host existence, online gate, then the addressed-Node call.
async fn proxy_to_host(
    state: &AppState,
    headers: &HeaderMap,
    host_id: &str,
    method: &str,
    node_params: Value,
) -> Result<Json<Value>, HubError> {
    require_origin(headers, &state.config)?;
    // Human or Bot only: require_operator refuses Agent origin with 403.
    crate::agent_scope::require_operator(state, headers).await?;
    if state.store.get_host(host_id.to_owned()).await?.is_none() {
        return Err(HubError::NotFound);
    }
    if state.nodes.kind_of(host_id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: host_id.to_owned(),
        });
    }
    let body = crate::http::call_node(state, host_id, method, node_params).await?;
    Ok(Json(body))
}

/// `POST /v1/hosts/{id}/files/objects` — stage bytes a Node read off disk.
///
/// Host-token authenticated: the token must resolve to the host named in the
/// path, so a host can only stage under its own key. Bytes are stored as
/// `application/octet-stream` regardless of the filename; download serves them
/// with `nosniff` and an attachment disposition.
async fn stage_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
    Query(query): Query<StageQuery>,
    body: Bytes,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let token_host = crate::objects::authenticated_host(&state, &headers)
        .await?
        .ok_or(HubError::Unauthenticated)?;
    if token_host != host_id {
        // A Node may only stage files for the host it authenticated as.
        return Err(HubError::Forbidden);
    }
    if state.store.get_host(host_id.clone()).await?.is_none() {
        return Err(HubError::NotFound);
    }
    if body.is_empty() {
        return Err(HubError::BadRequest("host file body is empty".into()));
    }
    let max_bytes = state.config.attachment_max_bytes;
    if body.len() > max_bytes {
        return Err(HubError::BadRequest(format!(
            "RESOURCE_LIMIT: host file is {} bytes; the limit is {max_bytes}",
            body.len()
        )));
    }
    let original_name = match query
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        None => None,
        // The Node already sends a bare basename; sanitise again in depth.
        Some(name) => Some(
            sanitize_attachment_name(name)
                .ok_or_else(|| HubError::BadRequest(format!("invalid file name {name}")))?,
        ),
    };
    let digest = crate::config::sha256_hex(&body);
    let record = state
        .store
        .insert_object(crate::store::NewObject {
            instance_id: staging_instance(&host_id),
            host_id: host_id.clone(),
            // Never type host files as a renderable media kind.
            media_type: "application/octet-stream".to_owned(),
            extension: "bin".to_owned(),
            original_name,
            digest,
            bytes: body.to_vec(),
            device_id: format!("host:{host_id}"),
            ttl_seconds: HOST_FILES_TTL_SECONDS,
            instance_budget: HOST_FILES_BUDGET,
        })
        .await
        .map_err(crate::objects::map_object_error)?;

    tracing::info!(
        object_id = %record.object_id,
        digest = %record.digest,
        bytes = record.byte_len,
        host_id = %host_id,
        "host_file.staged"
    );
    Ok(Json(json!({
        "objectId": record.object_id,
        "digest": record.digest,
        "size": record.byte_len,
    })))
}
