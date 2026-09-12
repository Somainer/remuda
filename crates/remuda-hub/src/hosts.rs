//! Host routes. Add host endpoints here, alongside the registry surface.

use axum::{
    Router,
    routing::{get, post},
};

use crate::{AppState, http, registry};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/hosts", get(http::list_hosts))
        // D-018: mint a single-use Node enroll token as an authenticated device.
        .route("/v1/hosts/enroll-token", post(http::mint_enroll_token))
        .merge(registry::routes())
}
