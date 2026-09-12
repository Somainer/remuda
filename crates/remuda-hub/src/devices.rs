//! Device list, revoke, and phone pairing-code flow.

use crate::AppState;
use crate::auth::{
    PAIR_CODE_ALPHABET, device_cookie, expired_device_cookie, hash_secret, require_device,
    require_origin, verify_secret,
};
use crate::config::now_rfc3339;
use crate::error::HubError;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};

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
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let revoking_self = device.id == id;
    let ok = state.store.delete_device(id).await?;
    if !ok {
        return Err(HubError::NotFound);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    if revoking_self {
        response.headers_mut().insert(
            header::SET_COOKIE,
            expired_device_cookie(state.config.cookie_secure)
                .parse()
                .map_err(|_| HubError::Internal("invalid expiry cookie".into()))?,
        );
    }
    Ok(response)
}

async fn issue_pair_code(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let expires = expires_rfc3339(600);
    for _ in 0..8 {
        let code = pairing_code()?;
        let hash = hash_secret(&code)?;
        if state
            .store
            .insert_pair_code(
                hash,
                code[..4].to_owned(),
                device.id.clone(),
                expires.clone(),
            )
            .await?
        {
            return Ok(Json(json!({"code": code, "expiresAt": expires})));
        }
    }
    Err(HubError::Internal(
        "pairing selector allocation failed".into(),
    ))
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
    let device = state
        .store
        .insert_device(body.device_name, hash, token[..16].to_owned())
        .await?;
    let cookie = device_cookie(&token, state.config.cookie_secure);
    let body = json!({
        "deviceId": device.id,
        "token": token,
        "name": device.name,
    });
    Ok((StatusCode::OK, [(header::SET_COOKIE, cookie)], Json(body)).into_response())
}

fn pairing_code() -> Result<String, HubError> {
    let mut bytes = [0; 8];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| HubError::Internal("pairing entropy unavailable".into()))?;
    let mut out = String::with_capacity(8);
    for b in bytes {
        // The 32-symbol alphabet divides 256 exactly, so this is unbiased.
        out.push(PAIR_CODE_ALPHABET[b as usize % PAIR_CODE_ALPHABET.len()] as char);
    }
    Ok(out)
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
