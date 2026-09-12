//! Hub authentication, host registry, routing, and embedded Web assets.
//!
//! The `remuda` composition root owns the `hub` subcommand. This crate exposes
//! [`serve`] / [`spawn`] so that CLI agent can call:
//!
//! ```text
//! remuda hub --data-dir … --listen 127.0.0.1:8080
//! ```

#![allow(missing_docs)] // handler types; public API is documented below.

mod auth;
mod config;
mod error;
mod http;
mod store;
mod web;
mod ws;

use crate::auth::resolve_bootstrap;
use crate::http::{
    create_instance, get_journal, healthz, list_hosts, list_instances, login, post_command,
};
use crate::store::Store;
use crate::ws::{Bus, NodeRegistry, follow_socket, node_socket};
use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::{get, post};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::oneshot;

pub use config::HubConfig;
pub use error::HubError;

/// Process-wide Hub state shared by HTTP and WS handlers.
#[derive(Clone)]
pub struct AppState {
    /// Frozen listen/auth config.
    pub config: Arc<HubConfig>,
    store: Store,
    nodes: NodeRegistry,
    bus: Bus,
}

/// A bound Hub that shuts down when dropped.
pub struct RunningHub {
    /// Actual listen address (port may be ephemeral).
    pub addr: SocketAddr,
    /// Bootstrap token devices and Nodes exchange on first enroll.
    pub bootstrap_token: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for RunningHub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Serve until Ctrl-C (composition-root entry).
pub async fn serve(config: HubConfig) -> anyhow::Result<()> {
    let running = spawn(config).await?;
    tracing::info!(addr = %running.addr, "remuda hub listening");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// Bind and spawn the Axum server. Used by tests and `remuda hub`.
pub async fn spawn(mut config: HubConfig) -> anyhow::Result<RunningHub> {
    std::fs::create_dir_all(&config.data_dir)?;
    resolve_bootstrap(&mut config)?;
    let bootstrap_token = config.bootstrap_token.clone();
    let store = Store::open(&config.data_dir)?;
    let state = AppState {
        config: Arc::new(config.clone()),
        store,
        nodes: NodeRegistry::default(),
        bus: Bus::new(),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let shutdown = async {
            let _ = rx.await;
        };
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::error!(error = %err, "hub server exited");
        }
    });
    Ok(RunningHub {
        addr,
        bootstrap_token,
        shutdown: Some(tx),
    })
}

/// Axum router (HTTP + WS + static).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/login", post(login))
        .route("/v1/hosts", get(list_hosts))
        .route("/v1/instances", get(list_instances).post(create_instance))
        .route("/v1/instances/{id}/commands", post(post_command))
        .route("/v1/instances/{id}/journal", get(get_journal))
        .route("/v1/follow", get(follow_socket))
        .route("/v1/node", get(node_socket))
        .route("/node/v1/connect", get(node_socket))
        .fallback(static_fallback)
        .with_state(state)
}

async fn static_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    web::static_handler(uri, state.config.web_root.clone()).await
}
