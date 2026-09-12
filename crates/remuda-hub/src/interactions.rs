//! HTTP surface for pending Interactions and first-answer-wins respond.

use crate::AppState;
use crate::auth::{require_device, require_origin};
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
}

/// `GET /v1/interactions` — durable Hub index, merged with live Node RPC.
pub async fn list_interactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    require_device(&state.store, &headers).await?;
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
                                    items.push(item.clone());
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
    let device = require_device(&state.store, &headers).await?;
    let interaction_id =
        InteractionId::try_from(id).map_err(|err| HubError::BadRequest(err.to_string()))?;
    let command_id = match body.command_id {
        Some(raw) => {
            CommandId::try_from(raw).map_err(|err| HubError::BadRequest(err.to_string()))?
        }
        None => CommandId::new(),
    };
    let by_device = Id::try_from(device.id).map_err(|err| HubError::Internal(err.to_string()))?;
    let stored = state
        .store
        .get_interaction(interaction_id.as_id().as_str().to_string())
        .await?;
    if let Some(row) = &stored {
        if row.state != "pending" {
            return Err(HubError::Superseded {
                winner: row
                    .payload
                    .get("answerCommandId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        let _ = state
            .store
            .answer_interaction(
                interaction_id.as_id().as_str().to_string(),
                command_id.as_id().as_str().to_string(),
            )
            .await?;
    }
    let had_stored = stored.is_some();
    let params = json!({
        "interactionId": interaction_id.as_id().as_str(),
        "commandId": command_id.as_id().as_str(),
        "byDevice": by_device.as_str(),
        "answer": body.answer,
    });
    let mut last_not_found = !had_stored;
    for host_id in state.nodes.host_ids().await {
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
                Ok(result) => return Ok(Json(result)),
                Err(HubError::NotFound) => {}
                Err(err) => return Err(err),
            },
            Ok(None) => last_not_found = true,
            Err(err) => return Err(err),
        }
    }
    if had_stored {
        let rec = state
            .store
            .get_interaction(interaction_id.as_id().as_str().to_string())
            .await?
            .ok_or(HubError::NotFound)?;
        return Ok(Json(rec.to_list_item()));
    }
    if last_not_found {
        return Err(HubError::NotFound);
    }
    Err(HubError::NotFound)
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
            -32602 => HubError::NotFound,
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
