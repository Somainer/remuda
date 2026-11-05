//! Remuda composition root: the unified CLI, and the generators the gate runs
//! against it.
//!
//! The binary is a thin wrapper ([`run`]); everything else lives here so the
//! clap tree can be introspected by [`cli_command`] and the machine half of the
//! agent-parity ledger ([`parity`], D-060 decision 12).

mod build_info;
mod cmd;
mod config;
pub mod parity;

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

/// The built `remuda` command tree, for generators that read the clap registry
/// (`crates/remuda/examples/gen_parity.rs`).
pub fn cli_command() -> clap::Command {
    use clap::CommandFactory;
    Cli::command()
}

/// Parse the process arguments and run the selected command.
pub fn run() -> anyhow::Result<()> {
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
        .map_err(|_| anyhow::anyhow!("cannot install tracing subscriber"))?;
    Ok(())
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

    #[cfg(unix)]
    async fn wait(&mut self) -> anyhow::Result<()> {
        let signal = tokio::select! {
            value = self.terminate.recv() => value,
            value = self.interrupt.recv() => value,
        };
        signal.context("process signal stream ended")?;
        tracing::info!("shutdown requested");
        Ok(())
    }

    #[cfg(not(unix))]
    async fn wait(&mut self) -> anyhow::Result<()> {
        tokio::signal::ctrl_c()
            .await
            .context("cannot receive Ctrl-C")?;
        tracing::info!("shutdown requested");
        Ok(())
    }
}
