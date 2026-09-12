//! Hub startup, configuration projection, and process shutdown ownership.
//!
//! The remuda-hub dependency enables `embed-web`; its build script embeds
//! web/dist when present and otherwise uses the crate's fallback page.

use crate::{Shutdown, config::Config, dispatcher};
use clap::Args as ClapArgs;
use std::{net::SocketAddr, path::PathBuf};

#[derive(ClapArgs)]
pub(crate) struct Args {
    /// Hub HTTP/WebSocket listener; overrides file/environment configuration.
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Serve assets from this directory before trying the embedded application.
    #[arg(long)]
    web_root: Option<PathBuf>,
    /// Run the configured Feishu dispatcher against this Hub in the same process.
    #[arg(long)]
    with_dispatcher: bool,
}

impl Args {
    pub fn apply(self, config: &mut Config) {
        if let Some(listen) = self.listen {
            config.hub.listen = listen;
        }
        if let Some(root) = self.web_root {
            config.hub.web_root = Some(root);
        }
    }
}

pub(crate) async fn start(config: &Config) -> anyhow::Result<remuda_hub::RunningHub> {
    let bootstrap_token = config
        .hub
        .bootstrap_token
        .as_ref()
        .map(|reference| reference.resolve().map(|secret| secret.into_string()))
        .transpose()?
        .unwrap_or_default();
    remuda_hub::spawn(remuda_hub::HubConfig {
        data_dir: config.data_dir.clone(),
        listen: config.hub.listen,
        bootstrap_token,
        cookie_secure: config.hub.cookie_secure,
        allowed_origins: config.hub.allowed_origins.clone(),
        web_root: config.hub.web_root.clone(),
        command_accept_timeout_ms: config.hub.command_accept_timeout_ms,
        create_settle_timeout_ms: config.hub.create_settle_timeout_ms,
        host_lost_grace_ms: config.hub.host_lost_grace_ms,
        ..remuda_hub::HubConfig::default()
    })
    .await
}

pub(crate) async fn run(
    mut config: Config,
    args: Args,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let with_dispatcher = args.with_dispatcher;
    args.apply(&mut config);
    config.validate()?;
    if with_dispatcher && config.dispatcher.is_none() {
        anyhow::bail!("--with-dispatcher requires a [dispatcher] configuration section");
    }
    let running = start(&config).await?;
    tracing::info!(address = %running.addr, "remuda hub listening");
    let result = if with_dispatcher {
        dispatcher::run_configured(&config, Some(&running), shutdown.wait()).await
    } else {
        shutdown.wait().await
    };
    // RunningHub::drop requests Axum graceful shutdown. TODO(remuda-hub): expose
    // an awaited shutdown handle so the CLI can also verify completion of its drain.
    drop(running);
    result
}
