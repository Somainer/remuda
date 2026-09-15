//! HTTP surface for pending Interactions and first-answer-wins respond.

use crate::AppState;
use crate::auth::require_origin;
use crate::error::HubError;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_protocol::{CommandId, Id, InteractionAnswer, InteractionId};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;

const NODE_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Context for an accepted Bot-relayed answer (design §5.1 #1).
struct BotRelay {
    /// Acting owner Feishu `open_id`.
    open_id: String,
    /// Open card ticket that authorized the relay.
    ticket_id: String,
}

/// REST routes for `/v1/interactions`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/interactions", get(list_interactions))
        .route("/v1/interactions/{id}/answer", post(answer_interaction))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerBody {
    #[serde(default)]
    command_id: Option<String>,
    answer: InteractionAnswer,
    /// Feishu `open_id` of the owner who acted. Required, and only consulted,
    /// when the caller is a Bot device relaying a card click (design §5.1 #1).
    #[serde(default)]
    acting_open_id: Option<String>,
}

/// `GET /v1/interactions` — durable Hub index, merged with live Node RPC.
pub async fn list_interactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let device = crate::agent_scope::require_operator(&state, &headers).await?;
    let mut items: Vec<Value> = state
        .store
        .list_interactions(
            query.host_id.clone(),
            query.instance_id.clone(),
            query.kind.clone(),
            true,
        )
        .await?
        .into_iter()
        .map(|row| row.to_list_item())
        .collect();
    items.extend(
        state
            .agent_approvals
            .list(&device)
            .await
            .into_iter()
            .filter(|item| {
                query
                    .host_id
                    .as_deref()
                    .is_none_or(|host| item["hostId"] == host)
                    && query
                        .instance_id
                        .as_deref()
                        .is_none_or(|id| item["instanceId"] == id)
                    && query.kind.as_deref().is_none_or(|kind| kind == "approval")
            }),
    );
    let mut seen: HashSet<String> = items
        .iter()
        .filter_map(|item| {
            item.get("interactionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let mut params = json!({});
    if let Some(instance_id) = &query.instance_id
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("instanceId".into(), json!(instance_id));
    }
    if let Some(kind) = &query.kind
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("kind".into(), json!(kind));
    }
    let hosts = match &query.host_id {
        Some(host_id) => vec![host_id.clone()],
        None => state.nodes.host_ids().await,
    };
    for host_id in hosts {
        match state
            .nodes
            .call(
                &host_id,
                "interaction.list",
                params.clone(),
                NODE_RPC_TIMEOUT,
            )
            .await
        {
            Ok(Some(frame)) => match rpc_result(frame) {
                Ok(result) => {
                    if let Some(batch) = result.get("items").and_then(Value::as_array) {
                        for item in batch {
                            if query.kind.as_ref().is_none_or(|want| {
                                item.get("kind").and_then(Value::as_str) == Some(want.as_str())
                            }) {
                                let id = item
                                    .get("interactionId")
                                    .or_else(|| item.get("id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if id.is_empty() || seen.insert(id.to_string()) {
                                    items.push(flatten_interaction(item.clone()));
                                }
                            }
                        }
                    }
                }
                Err(HubError::NotFound) => {}
                Err(err) => tracing::debug!(error = %err, %host_id, "interaction.list"),
            },
            Ok(None) => {}
            Err(err) => tracing::debug!(error = %err, %host_id, "interaction.list rpc"),
        }
    }
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `POST /v1/interactions/:id/answer` — unique commandId wins.
pub async fn answer_interaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let origin = crate::agent_scope::origin(&device);
    if origin == remuda_protocol::InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
    let interaction_id =
        InteractionId::try_from(id).map_err(|err| HubError::BadRequest(err.to_string()))?;
    // D-005/D-011 stay in force for Bot callers (design §5.1 #1): the bot is
    // only the owner's courier. A relay is accepted exactly when all hold:
    //   1. the answer carries the acting owner's Feishu open_id,
    //   2. that open_id is on this bot device's allowlist,
    //   3. an open, unexpired card ticket binds this interaction to the bot.
    // Agent-relayed approvals without these stay untrusted and get 403.
    let bot_relay = if origin == remuda_protocol::InputOrigin::Bot {
        let acting_open_id = body
            .acting_open_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or(HubError::Forbidden)?;
        if !state
            .store
            .bot_owner_allowlist_contains(device.id.clone(), acting_open_id.to_string())
            .await?
        {
            return Err(HubError::Forbidden);
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let ticket = state
            .store
            .get_open_card_ticket_for_bot(
                interaction_id.as_id().as_str().to_string(),
                device.id.clone(),
            )
            .await?
            .ok_or(HubError::Forbidden)?;
        if ticket.expires_at_ms <= now_ms {
            return Err(HubError::Forbidden);
        }
        Some(BotRelay {
            open_id: acting_open_id.to_string(),
            ticket_id: ticket.ticket_id,
        })
    } else {
        None
    };
    let command_id = match body.command_id {
        Some(raw) => {
            CommandId::try_from(raw).map_err(|err| HubError::BadRequest(err.to_string()))?
        }
        None => CommandId::new(),
    };
    let device_id = device.id.clone();
    let by_device = Id::try_from(device.id).map_err(|err| HubError::Internal(err.to_string()))?;
    if let Some(result) = state
        .agent_approvals
        .answer(
            &interaction_id,
            body.answer.clone(),
            by_device.clone(),
            command_id.clone(),
        )
        .await?
    {
        if let Some(relay) = &bot_relay {
            settle_bot_relay(
                &state,
                &device_id,
                relay,
                interaction_id.as_id().as_str(),
                command_id.as_id().as_str(),
            )
            .await;
        }
        return Ok(Json(result));
    }
    let stored = state
        .store
        .get_interaction(interaction_id.as_id().as_str().to_string())
        .await?;
    // Node owns first-answer-wins. Never commit an answer in the Hub before
    // the owner is reached, and let the Node reconcile same-command retries.
    let mut params = json!({
        "interactionId": interaction_id.as_id().as_str(),
        "commandId": command_id.as_id().as_str(),
        "byDevice": by_device.as_str(),
        "answer": body.answer,
    });
    if let Some(relay) = &bot_relay {
        // Informational only: the Node still authorizes against `byDevice`.
        // The durable attribution of the human owner lives in the Hub audit row.
        params["actingOpenId"] = json!(relay.open_id);
    }
    let hosts = match stored {
        Some(row) => vec![row.host_id],
        None => state.nodes.host_ids().await,
    };
    for host_id in hosts {
        match state
            .nodes
            .call(
                &host_id,
                "interaction.answer",
                params.clone(),
                NODE_RPC_TIMEOUT,
            )
            .await
        {
            Ok(Some(frame)) => match rpc_result(frame) {
                Ok(result) => {
                    // Mirror the owner's successful CAS immediately; a delayed
                    // journal flush must not resurrect a just-answered card.
                    state
                        .store
                        .record_interaction_answer(interaction_id.as_id().to_string())
                        .await?;
                    if let Some(relay) = &bot_relay {
                        settle_bot_relay(
                            &state,
                            &device_id,
                            relay,
                            interaction_id.as_id().as_str(),
                            command_id.as_id().as_str(),
                        )
                        .await;
                    }
                    return Ok(Json(result));
                }
                Err(HubError::NotFound) => {}
                Err(err) => return Err(err),
            },
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }

    Err(HubError::NotFound)
}

/// Winning-relay side effects: the Hub owns the authoritative ticket row, so
/// flip it to `answered` here (rather than trusting the dispatcher client),
/// and write the audit row that names the acting owner. D-005/D-011: the bot
/// never self-attests — the human `open_id` behind every relayed approval is
/// auditable and names the acting owner, not the bot device alone.
async fn settle_bot_relay(
    state: &AppState,
    device_id: &str,
    relay: &BotRelay,
    interaction_id: &str,
    command_id: &str,
) {
    if let Err(err) = state
        .store
        .set_card_ticket_state(
            relay.ticket_id.clone(),
            device_id.to_string(),
            "answered".into(),
            None,
        )
        .await
    {
        tracing::error!(error = %err, interaction_id, "failed to settle card ticket state");
    }
    if let Err(err) = state
        .store
        .append_audit(
            device_id.to_string(),
            "interaction.bot-answer".into(),
            Some(interaction_id.to_string()),
            json!({
                "actingOpenId": relay.open_id,
                "ticketId": relay.ticket_id,
                "commandId": command_id,
                "relayedBy": "feishu",
            }),
        )
        .await
    {
        tracing::error!(error = %err, interaction_id, "failed to audit bot-relayed answer");
    }
}

fn rpc_result(frame: Value) -> Result<Value, HubError> {
    if let Some(err) = frame.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("node error");
        return Err(match code {
            -32005 => HubError::Expired,
            -32004 => HubError::Superseded {
                winner: err
                    .pointer("/data/winner")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
            -32602 if message.contains("not found") => HubError::NotFound,
            -32602 => HubError::BadRequest(message.to_string()),
            _ if message.contains("expired") => HubError::Expired,
            _ if message.contains("already answered") => HubError::Superseded {
                winner: String::new(),
            },
            _ if message.contains("not found") => HubError::NotFound,
            _ => HubError::Internal(message.to_string()),
        });
    }
    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
}

// Retain the Node list wrapper for existing clients while exposing the complete
// Interaction entity at the top level consumed by the web API.
fn flatten_interaction(mut item: Value) -> Value {
    if let Some(entity) = item.get("interaction").and_then(Value::as_object).cloned()
        && let Some(object) = item.as_object_mut()
    {
        object.extend(entity);
    }
    item
}
