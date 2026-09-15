//! Bot-only HTTP surface (design §5.1 blockers 1 and 3).
//!
//! These routes are used by the Feishu dispatcher authenticating with a Bot
//! device token (D-017/D-018): registering the owner `open_id` allowlist its
//! relays are restricted to, and persisting card ↔ interaction tickets so a
//! dispatcher restart keeps open cards answerable.
//!
//! They are deliberately absent from `openapi/openapi.json`: the web client
//! never calls them and they sit beside Node RPC as internal Hub machinery.
//! Everything here fails closed — a missing allowlist entry is a 403, never a
//! silent allow, and the bot can never bypass approvals (D-005/D-011).

use crate::AppState;
use crate::agent_scope::{caller, origin};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::store_tickets::CardTicketRecord;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::post;
use remuda_protocol::{InputOrigin, U64};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

/// Routes under `/v1/bot`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/bot/owner-allowlist", post(put_owner_allowlist))
        .route(
            "/v1/bot/card-tickets",
            post(upsert_card_ticket).get(list_card_tickets),
        )
        .route(
            "/v1/bot/card-tickets/expire-session",
            post(expire_session_tickets),
        )
        .route("/v1/bot/card-tickets/{id}/state", post(set_ticket_state))
}

/// Resolve the Bot device or refuse. A bot route never accepts Human or Agent
/// tokens: owners answer directly, agents never answer approvals at all.
async fn require_bot(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::store::Device, HubError> {
    let device = caller(state, headers).await?;
    if origin(&device) != InputOrigin::Bot {
        return Err(HubError::Forbidden);
    }
    Ok(device)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllowlistBody {
    /// Feishu `open_id`s the dispatcher may answer on behalf of.
    owner_open_ids: Vec<String>,
}

/// `POST /v1/bot/owner-allowlist` — dispatcher registers its configured owners
/// on boot. Replaces the previous list; an empty list is allowed and fails
/// closed (every relayed answer is then refused).
async fn put_owner_allowlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AllowlistBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_bot(&state, &headers).await?;
    let mut ids = body.owner_open_ids;
    for id in &ids {
        if id.trim().is_empty() {
            return Err(HubError::BadRequest(
                "owner open ids must be non-empty".into(),
            ));
        }
    }
    ids.sort();
    ids.dedup();
    state
        .store
        .set_bot_owner_allowlist(device.id.clone(), &ids)
        .await?;
    Ok(Json(json!({ "deviceId": device.id, "ownerOpenIds": ids })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketBody {
    ticket_id: String,
    interaction_id: String,
    instance_id: String,
    session_key: String,
    #[serde(default)]
    card_message_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    request_version: U64,
    process_generation: U64,
    request_json: String,
    created_at_ms: i64,
    expires_at_ms: i64,
}

/// `POST /v1/bot/card-tickets` — upsert one ticket, stamped with the bot device.
async fn upsert_card_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TicketBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_bot(&state, &headers).await?;
    if body.ticket_id.trim().is_empty()
        || body.interaction_id.trim().is_empty()
        || body.instance_id.trim().is_empty()
        || body.session_key.trim().is_empty()
        || body.request_json.trim().is_empty()
    {
        return Err(HubError::BadRequest(
            "ticket id, interaction id, instance id, session key and request are required".into(),
        ));
    }
    if body.expires_at_ms <= body.created_at_ms {
        return Err(HubError::BadRequest(
            "ticket expires_at_ms must be after created_at_ms".into(),
        ));
    }
    let record = CardTicketRecord {
        ticket_id: body.ticket_id,
        device_id: device.id,
        interaction_id: body.interaction_id,
        instance_id: body.instance_id,
        session_key: body.session_key,
        card_message_id: body.card_message_id,
        state: body.state.unwrap_or_else(|| "open".into()),
        request_version: i64::try_from(body.request_version.0).unwrap_or(i64::MAX),
        process_generation: i64::try_from(body.process_generation.0).unwrap_or(i64::MAX),
        request_json: body.request_json,
        created_at_ms: body.created_at_ms,
        expires_at_ms: body.expires_at_ms,
    };
    state.store.upsert_card_ticket(record.clone()).await?;
    Ok(Json(ticket_json(&record)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketListQuery {
    /// Hydration cursor (unix ms); due tickets are excluded.
    now_ms: Option<i64>,
}

/// `GET /v1/bot/card-tickets` — open tickets for dispatcher restart hydration.
async fn list_card_tickets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TicketListQuery>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_bot(&state, &headers).await?;
    let tickets = state
        .store
        .list_open_card_tickets(device.id, query.now_ms.unwrap_or_else(now_ms))
        .await?;
    Ok(Json(json!({
        "tickets": tickets.iter().map(ticket_json).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketStateBody {
    state: String,
    #[serde(default)]
    card_message_id: Option<String>,
}

/// `POST /v1/bot/card-tickets/{id}/state`.
async fn set_ticket_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TicketStateBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_bot(&state, &headers).await?;
    if !matches!(body.state.as_str(), "open" | "answered" | "expired") {
        return Err(HubError::BadRequest(
            "ticket state must be open, answered, or expired".into(),
        ));
    }
    state
        .store
        .set_card_ticket_state(id, device.id, body.state, body.card_message_id)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpireSessionBody {
    session_key: String,
}

/// `POST /v1/bot/card-tickets/expire-session` — `/new`/`/stop` retire cards.
async fn expire_session_tickets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ExpireSessionBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_bot(&state, &headers).await?;
    let ids = state
        .store
        .expire_card_tickets_for_session(device.id, body.session_key)
        .await?;
    Ok(Json(json!({ "expiredTicketIds": ids })))
}

fn ticket_json(record: &CardTicketRecord) -> Value {
    json!({
        "ticketId": record.ticket_id,
        "interactionId": record.interaction_id,
        "instanceId": record.instance_id,
        "sessionKey": record.session_key,
        "cardMessageId": record.card_message_id,
        "state": record.state,
        "requestVersion": record.request_version.to_string(),
        "processGeneration": record.process_generation.to_string(),
        "requestJson": record.request_json,
        "createdAtMs": record.created_at_ms,
        "expiresAtMs": record.expires_at_ms,
    })
}
