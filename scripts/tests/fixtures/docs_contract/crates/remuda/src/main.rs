//! Fixture composition root.

mod cmd;

use clap::Parser;

#[derive(Parser)]
#[command(name = "remuda")]
struct Cli {
    #[command(flatten)]
    context: cmd::registry::Context,
    #[command(subcommand)]
    command: cmd::Command,
}
