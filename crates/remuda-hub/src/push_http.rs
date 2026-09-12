//! Nest `remuda_push::router` at `/push` behind device auth.

use crate::auth::require_device;
use crate::error::HubError;
use crate::store::Store;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use remuda_push::PushService;
use serde_json::{Value, json};

/// Authenticated `/push` fragment (config + subscriptions).
pub fn nest(push: PushService, store: Store) -> Router {
    remuda_push::router(push).layer(middleware::from_fn_with_state(store, push_auth))
}

async fn push_auth(
    State(store): State<Store>,
    mut req: Request,
    next: Next,
) -> Result<Response, HubError> {
    let device = require_device(&store, req.headers()).await?;
    if req.method() == Method::POST && req.uri().path().ends_with("/subscriptions") {
        req = stamp_device_id(req, &device.id).await?;
    }
    Ok(next.run(req).await)
}

async fn stamp_device_id(req: Request, device_id: &str) -> Result<Request, HubError> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 256 * 1024)
        .await
        .map_err(|err| HubError::BadRequest(err.to_string()))?;
    let mut value: Value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}))
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("deviceId".into(), json!(device_id));
    }
    let mut req = Request::from_parts(parts, Body::from(value.to_string()));
    req.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Ok(req)
}
