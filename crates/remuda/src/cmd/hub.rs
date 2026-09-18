//! Hub startup, configuration projection, and process shutdown ownership.
//!
//! The remuda-hub dependency enables `embed-web`; its build script embeds
//! web/dist when present and otherwise uses the crate's fallback page.

use crate::{Shutdown, config::Config, dispatcher};
use clap::{Args as ClapArgs, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(ClapArgs)]
#[command(about = "Run the Hub, authentication store, and embedded Web application.")]
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
    /// Probe the configured local Hub health endpoint and exit (container healthcheck).
    #[arg(long, conflicts_with_all = ["migrate", "with_dispatcher"])]
    healthcheck: bool,
    /// Apply the Hub's SQLite schema updates and exit without starting a listener.
    #[arg(long, conflicts_with = "with_dispatcher")]
    migrate: bool,
    #[command(subcommand)]
    command: Option<HubCommand>,
}

/// Hub maintenance that runs without serving (D-018).
#[derive(Subcommand)]
pub(crate) enum HubCommand {
    /// Replace the device pairing access code and print the new one.
    ///
    /// Paired devices keep their tokens; only future pairing is affected.
    RotateBootstrap,
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

/// `remuda hub rotate-bootstrap`: mint a new device pairing access code.
fn rotate_bootstrap(config: &Config) -> anyhow::Result<()> {
    let token = remuda_hub::rotate_bootstrap(&config.data_dir)?;
    tracing::info!(
        path = %config.data_dir.join("bootstrap-token").display(),
        "rotated device pairing access code"
    );
    println!("{token}");
    Ok(())
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
        public_origin: config.hub.public_origin.clone(),
        trusted_proxies: config.hub.trusted_proxies.clone(),
        allowed_origins: config.hub.allowed_origins.clone(),
        web_root: config.hub.web_root.clone(),
        command_accept_timeout_ms: config.hub.command_accept_timeout_ms,
        create_settle_timeout_ms: config.hub.create_settle_timeout_ms,
        command_settle_timeout_ms: config.hub.command_settle_timeout_ms,
        host_lost_grace_ms: config.hub.host_lost_grace_ms,
        // 0 means "use the Hub default" (25 MiB, D-027b).
        attachment_max_bytes: if config.hub.attachment_max_bytes == 0 {
            remuda_hub::DEFAULT_ATTACHMENT_MAX_BYTES
        } else {
            config.hub.attachment_max_bytes
        },
        ..remuda_hub::HubConfig::default()
    })
    .await
}

pub(crate) async fn run(
    mut config: Config,
    args: Args,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    if let Some(HubCommand::RotateBootstrap) = args.command {
        config.validate()?;
        return rotate_bootstrap(&config);
    }
    let with_dispatcher = args.with_dispatcher;
    let healthcheck = args.healthcheck;
    let migrate = args.migrate;
    args.apply(&mut config);
    config.validate()?;
    if healthcheck {
        return super::hub_maintenance::healthcheck(config.hub.listen).await;
    }
    if migrate {
        return remuda_hub::migrate(&config.data_dir).await;
    }
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

impl super::registry::Entrypoint for Args {
    fn enter(self, context: super::registry::Context) -> anyhow::Result<i32> {
        super::registry::service(context, |config, shutdown| run(config, self, shutdown))
    }
}
