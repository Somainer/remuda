//! Instance routes. New endpoints and handlers belong in this feature module.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{AppState, HubError, http};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/instances",
            get(http::list_instances).post(http::create_instance),
        )
        .route(
            "/v1/instances/{id}",
            get(http::get_instance).delete(http::delete_instance),
        )
        .route(
            "/v1/instances/{id}/commands",
            get(http::list_instance_commands).post(http::post_command),
        )
        .route(
            "/v1/instances/{id}/commands/{commandId}",
            get(http::get_instance_command),
        )
        .route("/v1/instances/{id}/resume", post(http::resume_instance))
        .route("/v1/instances/{id}/seen", post(mark_seen))
        .route("/v1/lineages/{id}", get(http::get_lineage))
        .route("/v1/instances/{id}/journal", get(http::get_journal))
        .route("/v1/instances/{id}/screen", get(http::get_screen))
        .route(
            "/v1/instances/{id}/subagents/{agentId}/transcript",
            get(http::get_subagent_transcript),
        )
}

/// A durable seq as the Hub serializes it (decimal string) or as a JSON
/// integer.
#[derive(Deserialize)]
#[serde(untagged)]
enum SeqInput {
    Text(String),
    Number(u64),
}

impl SeqInput {
    fn value(&self) -> Result<i64, HubError> {
        let parsed = match self {
            Self::Text(text) => text.trim().parse::<u64>().ok(),
            Self::Number(number) => Some(*number),
        };
        parsed
            .and_then(|seq| i64::try_from(seq).ok())
            .ok_or_else(|| HubError::BadRequest("durableSeq must be a non-negative seq".into()))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeenBody {
    /// Highest journal seq the client has shown for this instance.
    durable_seq: SeqInput,
    /// `true` while the client is showing the session (renewed inside
    /// [`crate::seen::VISIBLE_LEASE`]), `false` when it stops; absent for a
    /// report that is not about the screen (e.g. "mark all seen").
    #[serde(default)]
    visible: Option<bool>,
}

/// `POST /v1/instances/{id}/seen` — the owner saw this instance up to
/// `durableSeq` (seen-hub).
///
/// Monotonic: a lower seq never moves seen back, and a seq past the journal is
/// clamped to the instance's durable seq. Human devices only: the agent-route
/// middleware already refuses Agent credentials (this path is on no agent
/// allowlist), and a Bot is refused here — the seen seq is what the owner
/// looked at, which only the owner's own devices can claim.
async fn mark_seen(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Json(body): Json<SeenBody>,
) -> Result<Json<Value>, HubError> {
    crate::auth::require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    if crate::agent_scope::origin(&device) != remuda_protocol::InputOrigin::Human {
        return Err(HubError::Forbidden);
    }
    let seq = body.durable_seq.value()?;
    let marked = state
        .store
        .report_seen(instance_id, &device.id, seq, body.visible)
        .await
        .map_err(http::map_store)?
        .ok_or(HubError::NotFound)?;
    if marked.advanced {
        crate::ws::publish_seen(&state.bus, &marked.view);
    }
    Ok(Json(
        serde_json::to_value(&marked.view).map_err(|err| HubError::Internal(err.to_string()))?,
    ))
}
