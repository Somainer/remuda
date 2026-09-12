//! Host routes. Add host endpoints here, alongside the registry surface.

use axum::{Router, routing::get};

use crate::{AppState, http, registry};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/hosts", get(http::list_hosts))
        .merge(registry::routes())
}
