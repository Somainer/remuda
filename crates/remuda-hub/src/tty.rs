//! Browser follow/TTY routes. The follow socket multiplexes events and terminal frames.

use axum::{Router, routing::get};

use crate::{AppState, ws};

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/follow", get(ws::follow_socket))
}
