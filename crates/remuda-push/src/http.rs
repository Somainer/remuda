//! Axum fragment the Hub mounts at `/push`.

use crate::PushService;
use crate::store::{Subscription, new_id};
use crate::validate::validate_subscription;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// JSON for `GET /push/config` (herdrx field name).
#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    /// Uncompressed P-256 public key, URL-safe base64.
    pub public_key: String,
}

/// Browser `PushSubscription.toJSON()` plus optional device id.
#[derive(Debug, Deserialize)]
pub struct SubscribeRequest {
    /// Push service endpoint.
    pub endpoint: String,
    /// Browser keys.
    pub keys: SubscribeKeys,
    /// Device that owns this subscription.
    #[serde(default, alias = "deviceId")]
    pub device_id: Option<String>,
}

/// `p256dh` + `auth` from the browser.
#[derive(Debug, Deserialize)]
pub struct SubscribeKeys {
    /// Uncompressed P-256 public key, URL-safe base64.
    pub p256dh: String,
    /// 16-byte auth secret, URL-safe base64.
    pub auth: String,
}

/// Body or query for `DELETE /push/subscriptions`.
#[derive(Debug, Default, Deserialize)]
pub struct UnsubscribeRequest {
    /// Endpoint to drop.
    pub endpoint: Option<String>,
}

/// Hub-mountable router: `GET /config`, `POST|DELETE /subscriptions`.
pub fn router(service: PushService) -> Router {
    Router::new()
        .route("/config", get(get_config))
        .route("/subscriptions", post(subscribe).delete(unsubscribe))
        .with_state(service)
}

async fn get_config(State(service): State<PushService>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        public_key: service.public_key().to_owned(),
    })
}

async fn subscribe(
    State(service): State<PushService>,
    Json(body): Json<SubscribeRequest>,
) -> Response {
    if body.endpoint.is_empty() || body.keys.p256dh.is_empty() || body.keys.auth.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid_subscription"})),
        )
            .into_response();
    }
    if let Err(err) = validate_subscription(
        &body.endpoint,
        &body.keys.p256dh,
        &body.keys.auth,
        service.validate_opts(),
    ) {
        tracing::debug!(?err, "rejected push subscription");
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid_subscription"})),
        )
            .into_response();
    }
    let device_id = body
        .device_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".into());
    let sub = Subscription {
        id: new_id(),
        device_id,
        endpoint: body.endpoint,
        p256dh: body.keys.p256dh,
        auth: body.keys.auth,
    };
    match service.upsert(sub) {
        Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(err) => {
            tracing::error!(?err, "failed to persist push subscription");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"database_error"})),
            )
                .into_response()
        }
    }
}

async fn unsubscribe(
    State(service): State<PushService>,
    Query(query): Query<UnsubscribeRequest>,
) -> Response {
    let Some(endpoint) = query.endpoint.filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid_subscription"})),
        )
            .into_response();
    };
    match service.unsubscribe(&endpoint) {
        Ok(true) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not_found"})),
        )
            .into_response(),
        Err(err) => {
            tracing::error!(?err, "failed to delete push subscription");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"database_error"})),
            )
                .into_response()
        }
    }
}
