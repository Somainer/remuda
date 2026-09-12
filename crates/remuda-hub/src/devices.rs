//! Device list, revoke, and phone pairing-code flow.

use crate::AppState;
use crate::auth::{device_cookie, hash_secret, require_device, require_origin, verify_secret};
use crate::config::now_rfc3339;
use crate::error::HubError;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// Device management routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/pair-code", post(issue_pair_code))
        .route("/v1/devices/pair", post(redeem_pair_code))
        .route("/v1/devices/{id}", delete(revoke_device))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairRedeemBody {
    code: String,
    #[serde(default = "default_phone_name")]
    device_name: String,
}

fn default_phone_name() -> String {
    "phone".into()
}

async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
    let items = state.store.list_devices().await?;
    Ok(Json(json!({ "items": items })))
}

async fn revoke_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let ok = state.store.delete_device(id).await?;
    if !ok {
        return Err(HubError::NotFound);
    }
    Ok(Json(json!({ "ok": true })))
}

async fn issue_pair_code(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let code = pairing_code();
    let hash = hash_secret(&code)?;
    let expires = expires_rfc3339(600);
    state
        .store
        .insert_pair_code(hash, device.id, expires.clone())
        .await?;
    Ok(Json(json!({
        "code": code,
        "expiresAt": expires,
    })))
}

async fn redeem_pair_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PairRedeemBody>,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    let now = now_rfc3339();
    let code = body.code.trim().to_uppercase();
    let ok = state
        .store
        .consume_pair_code(code.clone(), now, verify_secret)
        .await?;
    if !ok {
        return Err(HubError::Unauthenticated);
    }
    let token = crate::config::random_token();
    let hash = hash_secret(&token)?;
    let device = state.store.insert_device(body.device_name, hash).await?;
    let cookie = device_cookie(&token, state.config.cookie_secure);
    let body = json!({
        "deviceId": device.id,
        "token": token,
        "name": device.name,
    });
    Ok((StatusCode::OK, [(header::SET_COOKIE, cookie)], Json(body)).into_response())
}

fn pairing_code() -> String {
    const ALPH: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let raw = Uuid::new_v4();
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(8);
    for b in bytes.iter().take(8) {
        out.push(ALPH[(*b as usize) % ALPH.len()] as char);
    }
    out
}

fn expires_rfc3339(secs: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}
