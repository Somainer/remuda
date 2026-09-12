//! Hub authentication, host registry, routing, and embedded Web assets.
//!
//! The `remuda` composition root owns the `hub` subcommand. This crate exposes
//! [`serve`] / [`spawn`] so that CLI agent can call:
//!
//! ```text
//! remuda hub --data-dir … --listen 127.0.0.1:8080
//! ```

#![allow(missing_docs)] // handler types; public API is documented below.

mod alerts;
mod auth;
mod config;
mod devices;
mod error;
mod fleet;
mod http;
mod interactions;
mod inventory;
mod placement;
mod push_http;
mod registry;
mod store;
mod transport;
mod web;
mod ws;

use crate::alerts::{BlockedWatch, Followers};
use crate::auth::{persist_listen, resolve_bootstrap};
use crate::store::Store;
use crate::ws::Bus;
use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use remuda_push::{OpenOptions, PushService};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::oneshot;

pub use config::{DEFAULT_COMMAND_ACCEPT_TIMEOUT_MS, HubConfig, MIN_CREATE_SETTLE_TIMEOUT_MS};
pub use error::HubError;
pub use transport::{ConnectedNodes, NodeTransport, StdioTransport, TransportKind, WssTransport};

/// Process-wide Hub state shared by HTTP and WS handlers.
#[derive(Clone)]
pub struct AppState {
    /// Frozen listen/auth config.
    pub config: Arc<HubConfig>,
    store: Store,
    nodes: ConnectedNodes,
    bus: Bus,
    push: Option<PushService>,
    followers: Followers,
    blocked: BlockedWatch,
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
pub async fn spawn(config: HubConfig) -> anyhow::Result<RunningHub> {
    spawn_inner(config, None).await
}

/// Spawn with a fake Web Push transport (tests).
pub async fn spawn_with_push(
    config: HubConfig,
    transport: Arc<dyn remuda_push::Transport>,
) -> anyhow::Result<RunningHub> {
    spawn_inner(config, Some(transport)).await
}

async fn spawn_inner(
    mut config: HubConfig,
    transport: Option<Arc<dyn remuda_push::Transport>>,
) -> anyhow::Result<RunningHub> {
    std::fs::create_dir_all(&config.data_dir)?;
    resolve_bootstrap(&mut config)?;
    let bootstrap_token = config.bootstrap_token.clone();
    let store = Store::open(&config.data_dir)?;
    let push = match transport {
        Some(t) => Some(PushService::open_with(
            &config.data_dir,
            OpenOptions::test(t),
        )?),
        None => match PushService::open(&config.data_dir) {
            Ok(svc) => Some(svc),
            Err(err) => {
                tracing::warn!(error = %err, "web push disabled");
                None
            }
        },
    };
    let state = AppState {
        config: Arc::new(config.clone()),
        store,
        nodes: crate::transport::ConnectedNodes::default(),
        bus: Bus::with_capacity(config.follow_buffer_events),
        push,
        followers: Followers::default(),
        blocked: BlockedWatch::default(),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let addr = listener.local_addr()?;
    persist_listen(&config.data_dir, addr)?;
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
    let mut app = Router::new()
        .merge(http::routes())
        .merge(interactions::routes())
        .merge(ws::routes())
        .merge(registry::routes())
        .merge(placement::routes())
        .merge(fleet::routes())
        .merge(devices::routes());
    if let Some(push) = state.push.clone() {
        app = app.nest_service("/push", push_http::nest(push, state.store.clone()));
    }
    app.fallback(static_fallback).with_state(state)
}

async fn static_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    web::static_handler(uri, state.config.web_root.clone()).await
}
