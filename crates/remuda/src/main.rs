//! Remuda composition root. Operational command modules remain independently owned.

mod build_info;
mod cmd;
mod config;
use anyhow::Context;
use clap::Parser;
use cmd::{dispatcher, hub, node};

#[derive(Parser)]
#[command(name = "remuda", version, about = "Unified remote agent runtime")]
struct Cli {
    #[command(flatten)]
    context: cmd::registry::Context,
    #[command(subcommand)]
    command: cmd::Command,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.command.tracing() {
        init_tracing()?;
    }
    let code = cli.command.run(cli.context)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn init_tracing() -> anyhow::Result<()> {
    let directive = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "info".to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("RUST_LOG is not valid UTF-8"),
    };
    let filter = tracing_subscriber::EnvFilter::try_new(directive)
        .map_err(|_| anyhow::anyhow!("invalid RUST_LOG filter"))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .map_err(|_| anyhow::anyhow!("cannot install tracing subscriber"))
}

/// Install handlers before binding listeners; every process mode owns its cleanup.
struct Shutdown {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl Shutdown {
    fn install() -> anyhow::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("cannot install SIGTERM handler")?,
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("cannot install SIGINT handler")?,
        })
    }

    async fn wait(&mut self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            let signal = tokio::select! {
                value = self.terminate.recv() => value,
                value = self.interrupt.recv() => value,
            };
            signal.context("process signal stream ended")?;
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c()
            .await
            .context("cannot receive Ctrl-C")?;
        tracing::info!("shutdown requested");
        Ok(())
    }
}
