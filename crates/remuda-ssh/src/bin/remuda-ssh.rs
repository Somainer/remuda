//! Standalone `remuda-ssh` CLI (same flags as `remuda ssh`).
//!
//! Used when the composition-root `remuda` binary cannot be built.

use clap::Parser;
use remuda_ssh::SshArgs;

#[derive(Parser)]
#[command(
    name = "remuda-ssh",
    version,
    about = "SSH remote transport for Remuda"
)]
struct Cli {
    #[command(flatten)]
    args: SshArgs,
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    if let Err(error) = remuda_ssh::run_blocking(Cli::parse().args) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
