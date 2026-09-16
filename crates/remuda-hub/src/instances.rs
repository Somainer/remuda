//! Instance routes. New endpoints and handlers belong in this feature module.

use axum::{
    Router,
    routing::{get, post},
};

use crate::{AppState, http};

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
        .route("/v1/instances/{id}/commands", post(http::post_command))
        .route("/v1/instances/{id}/resume", post(http::resume_instance))
        .route("/v1/instances/{id}/journal", get(http::get_journal))
        .route("/v1/instances/{id}/screen", get(http::get_screen))
        .route(
            "/v1/instances/{id}/subagents/{agentId}/transcript",
            get(http::get_subagent_transcript),
        )
}
