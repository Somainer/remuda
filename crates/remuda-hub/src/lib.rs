//! Hub authentication, host registry, routing, and embedded Web assets.
//!
//! The `remuda` composition root owns the `hub` subcommand. This crate exposes
//! [`serve`] / [`spawn`] so that CLI agent can call:
//!
//! ```text
//! remuda hub --data-dir … --listen 127.0.0.1:8080
//! ```

#![allow(missing_docs)] // handler types; public API is documented below.

pub mod agent_approvals;
mod agent_scope;
mod alerts;
mod attachments;
mod auth;
mod bot;
mod config;
mod devices;
mod error;
mod fleet;
mod hosts;
mod http;
mod instances;
mod interactions;
mod inventory;
mod maintenance;
mod model_catalog;
/// Built-in model catalog revision (tests compare the served catalog against it).
pub use model_catalog::CATALOG_REVISION;
mod objects;
mod passkeys;
mod placement;
mod projects;
mod provider_models;
mod provider_resolve;
mod providers;
mod proxy;
mod push_http;
mod rate_limit;
mod registry;
pub mod ssh_hosts;
mod store;
mod store_tickets;
mod supply;
mod tasks;
mod transport;
mod tty;
mod usage_store;
mod web;
mod workspaces;
mod ws;

use crate::alerts::{BlockedWatch, Followers};
use crate::auth::{persist_listen, resolve_bootstrap};
use crate::store::Store;
use crate::ws::Bus;
use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use remuda_driver::FileSecretStore;
use remuda_push::{OpenOptions, PushService};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use agent_scope::instance_token;
pub use auth::{bootstrap_issued_at, rotate_bootstrap};
pub use config::{
    DEFAULT_BOOTSTRAP_TTL_HOURS, DEFAULT_COMMAND_ACCEPT_TIMEOUT_MS,
    DEFAULT_ENROLL_TOKEN_TTL_MINUTES, HubConfig, MIN_CREATE_SETTLE_TIMEOUT_MS,
};
pub use error::HubError;
pub use maintenance::migrate;
pub use transport::{ConnectedNodes, NodeTransport, StdioTransport, TransportKind, WssTransport};

/// Process-wide Hub state shared by HTTP and WS handlers.
#[derive(Clone)]
pub struct AppState {
    /// Frozen listen/auth config.
    pub config: Arc<HubConfig>,
    store: Store,
    /// Envelope-encrypted provider tokens under `data_dir/secrets`.
    secrets: Arc<FileSecretStore>,
    nodes: ConnectedNodes,
    bus: Bus,
    tty: crate::ws::TtyRelay,
    push: Option<PushService>,
    followers: Followers,
    blocked: BlockedWatch,
    auth_limits: rate_limit::AuthRateLimits,
    agent_approvals: agent_approvals::AgentApprovals,
    /// Process-local, single-use WebAuthn ceremony challenges (D-030).
    challenges: passkeys::ChallengeStore,
}

/// Test-only constructors for types private modules would otherwise hide
/// from integration tests. Not part of the supported API.
#[doc(hidden)]
pub mod store_test_support {
    use crate::store::InstanceDelegation;

    /// A leaf-worker delegation scoped to one project.
    pub fn leaf_delegation(project_id: &str) -> Result<InstanceDelegation, String> {
        let project = remuda_protocol::ProjectId::try_from(project_id.to_string())
            .map_err(|err| err.to_string())?;
        Ok(InstanceDelegation {
            role: Some(remuda_protocol::ROLE_WORKER.into()),
            scope: remuda_protocol::InstanceScope {
                project_ids: vec![project],
                ..Default::default()
            },
            grants: Vec::new(),
            task_id: None,
            enforce_tree: true,
        })
    }
}

/// A bound Hub that shuts down when dropped.
pub struct RunningHub {
    /// Actual listen address (port may be ephemeral).
    pub addr: SocketAddr,
    /// Bootstrap token devices and Nodes exchange on first enroll.
    pub bootstrap_token: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    store: Option<Store>,
    /// Live in-process state, for tests that register a synthetic Node.
    state: AppState,
}

impl RunningHub {
    /// Borrow this Hub's store (audit queries, support tooling, tests).
    #[must_use]
    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// Test helper: insert an online host row directly (no WS enroll).
    #[doc(hidden)]
    pub async fn test_insert_host(&self, host_id: &str) -> anyhow::Result<()> {
        if let Some(store) = self.store.as_ref() {
            let host_id = host_id.to_owned();
            store
                .run(move |conn| {
                    let now = crate::config::now_rfc3339();
                    conn.execute(
                        "INSERT OR REPLACE INTO hosts
                            (id, label, token_hash, state, last_seen_at, node_version, cli_json,
                             capabilities_json, created_at, transport, labels_json, herdr_json,
                             resources_json, max_instances, hostname, token_prefix)
                         VALUES (?1, ?1, 'x', 'online', ?2, '0.1.0-test', '[]', '{}', ?2,
                                 'outbound-wss', '[]', NULL, NULL, 8, 'scripted-node', NULL)",
                        rusqlite::params![host_id, now],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }

    /// Test helper: mint an instance-scoped agent credential (operator-only
    /// routes must reject it with 403).
    #[doc(hidden)]
    pub async fn test_mint_agent_token(
        &self,
        device_name: &str,
        instance_id: &str,
    ) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = config::random_token();
        let hash = auth::hash_secret(&token)?;
        let prefix = auth::token_prefix(&token)
            .ok_or_else(|| anyhow::anyhow!("generated device token is not indexable"))?
            .to_owned();
        store
            .insert_device_as(
                device_name.into(),
                hash,
                prefix,
                "agent".into(),
                Some(instance_id.into()),
            )
            .await?;
        Ok(token)
    }

    /// Test helper: replace a host's live Node transport with a synthetic one
    /// whose every RPC returns `reply` (the full JSON-RPC frame; `None` models
    /// an unwritable session → 422). Insert the host row first.
    #[doc(hidden)]
    pub async fn test_set_node_reply(&self, host_id: &str, reply: Option<serde_json::Value>) {
        use std::sync::Arc as StdArc;
        self.state
            .nodes
            .insert(
                host_id.to_owned(),
                StdArc::new(crate::transport::ScriptedTransport::new(reply)),
            )
            .await;
    }

    /// Test helper: drop the live Node session for `host_id` (host goes offline).
    #[doc(hidden)]
    pub async fn test_disconnect_node(&self, host_id: &str) {
        self.state.nodes.remove(host_id).await;
    }

    /// Mint a scoped device token against this Hub's store (D-018).
    ///
    /// In-process equivalent of `POST /v1/login`, for components composed into
    /// the same process as the Hub (`hub --with-dispatcher`, `remuda dev`).
    /// They get a real, revocable device token instead of holding the pairing
    /// access code, which under D-018 pairs devices and nothing else.
    pub async fn mint_device_token(&self, device_name: &str) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = config::random_token();
        let hash = auth::hash_secret(&token)?;
        let prefix = auth::token_prefix(&token)
            .ok_or_else(|| anyhow::anyhow!("generated device token is not indexable"))?
            .to_owned();
        store
            .insert_device(device_name.to_owned(), hash, prefix)
            .await
            .map_err(|err| anyhow::anyhow!("mint device token: {err}"))?;
        Ok(token)
    }

    /// Mint a single-use Node enroll token against this Hub's store (D-018).
    ///
    /// In-process equivalent of `POST /v1/hosts/enroll-token`, used by
    /// `remuda dev` to enroll its own local Node without the device access code.
    pub async fn mint_enroll_token(&self, ttl_minutes: u64) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = config::random_token();
        let hash = auth::hash_secret(&token)?;
        let expires = time::OffsetDateTime::now_utc()
            + time::Duration::minutes(i64::try_from(ttl_minutes.max(1)).unwrap_or(60));
        let expires_at = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            expires.year(),
            u8::from(expires.month()),
            expires.day(),
            expires.hour(),
            expires.minute(),
            expires.second(),
            expires.millisecond()
        );
        store
            .insert_enroll_token(
                hash,
                auth::token_prefix(&token).map(str::to_string),
                "dev-local".into(),
                expires_at,
            )
            .await
            .map_err(|err| anyhow::anyhow!("mint enroll token: {err}"))?;
        Ok(token)
    }

    /// Stop HTTP/WS accept and wait for the SQLite writer thread to close.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.task.take() {
            tokio::select! {
                _ = &mut task => {}
                () = tokio::time::sleep(Duration::from_secs(5)) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        if let Some(store) = self.store.take() {
            store.close().await;
        }
    }
}

impl Drop for RunningHub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.store.take();
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
    proxy::configure_public_origin(&mut config)?;
    std::fs::create_dir_all(&config.data_dir)?;
    resolve_bootstrap(&mut config)?;
    let bootstrap_token = config.bootstrap_token.clone();
    let store = Store::open(&config.data_dir)?;
    store
        .mark_all_hosts_offline()
        .await
        .map_err(|err| anyhow::anyhow!("mark hosts offline: {err}"))?;
    let secrets = FileSecretStore::open(config.data_dir.join("secrets"))
        .map_err(|err| anyhow::anyhow!("provider secrets: {err}"))?;
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
        store: store.clone(),
        secrets: Arc::new(secrets),
        nodes: crate::transport::ConnectedNodes::default(),
        bus: Bus::with_capacity(config.follow_buffer_events),
        tty: crate::ws::TtyRelay::default(),
        push,
        followers: Followers::default(),
        blocked: BlockedWatch::default(),
        auth_limits: rate_limit::AuthRateLimits::new(rate_limit::LimitParams {
            ip_burst: config.auth_ip_burst,
            ip_refill_per_sec: config.auth_ip_refill_per_sec,
            global_burst: config.auth_global_burst,
            global_refill_per_sec: config.auth_global_refill_per_sec,
        }),
        agent_approvals: agent_approvals::AgentApprovals::new()?,
        challenges: passkeys::ChallengeStore::default(),
    };
    store.expire_lost_hosts(config.host_lost_grace_ms).await?;
    // A Hub restart must not inherit yesterday's unacknowledged creates: they
    // would keep holding placement slots with no Node that can ever settle them.
    expire_stale_requested(&state, config.requested_grace_ms).await;
    let reaper_state = state.clone();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let addr = listener.local_addr()?;
    persist_listen(&config.data_dir, addr)?;
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let shutdown = async {
            let _ = rx.await;
        };
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .into_future();
        tokio::pin!(server);
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                result = &mut server => {
                    if let Err(err) = result { tracing::error!(error = %err, "hub server exited"); }
                    break;
                }
                _ = interval.tick() => {
                    if let Err(err) = reaper_state.store.expire_lost_hosts(config.host_lost_grace_ms).await {
                        tracing::error!(error = %err, "host-lost sweep failed");
                    }
                    expire_stale_requested(&reaper_state, config.requested_grace_ms).await;
                }
            }
        }
    });
    Ok(RunningHub {
        addr,
        bootstrap_token,
        shutdown: Some(tx),
        task: Some(task),
        store: Some(store),
        state,
    })
}

/// Fail `requested` instances the Node never acknowledged.
///
/// Each expiry gets a Hub-authored journal diagnostic so the row explains
/// itself, and stops counting toward the host's `maxInstances` ceiling.
async fn expire_stale_requested(state: &AppState, window_ms: u64) {
    let expired = match state.store.expire_stale_requested(window_ms).await {
        Ok(expired) => expired,
        Err(error) => {
            tracing::error!(%error, "stale-requested sweep failed");
            return;
        }
    };
    for (host_id, instance_id) in expired {
        tracing::warn!(
            %host_id,
            %instance_id,
            window_ms,
            "create was never acknowledged by the node; expiring to failed"
        );
        crate::ws::publish_hub_diagnostic(
            state,
            &instance_id,
            "create_never_acknowledged",
            "create was never acknowledged by the node; expired to failed",
        )
        .await;
    }
}

/// Axum router (HTTP + WS + static).
pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .merge(http::routes())
        .merge(hosts::routes())
        .merge(ssh_hosts::routes(state.clone()))
        .merge(instances::routes())
        .merge(interactions::routes())
        .merge(tty::routes())
        .merge(ws::routes())
        .merge(placement::routes())
        .merge(projects::routes())
        .merge(tasks::routes())
        .merge(fleet::routes())
        .merge(devices::routes())
        .merge(passkeys::routes())
        .merge(providers::routes())
        .merge(supply::routes())
        .merge(agent_scope::routes())
        .merge(objects::routes())
        .merge(attachments::routes())
        .merge(workspaces::routes())
        .merge(bot::routes());
    if let Some(push) = state.push.clone() {
        app = app.nest_service("/push", push_http::nest(push, state.store.clone()));
    }
    app.fallback(static_fallback)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            agent_scope::restrict_agent_routes,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::limit_auth_attempts,
        ))
        .layer(axum::middleware::map_response(web::security_headers))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            proxy::secure_cookies,
        ))
        .with_state(state)
}

async fn static_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    web::static_handler(uri, state.config.web_root.clone()).await
}
