//! Fixture demo command.

use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
#[command(about = "Exercise the docs-contract fixture.")]
pub(crate) struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    shared: SharedArgs,
}

/// Shared arguments, flattened into the demo command.
#[derive(ClapArgs)]
pub(crate) struct SharedArgs {
    /// Print more detail.
    #[arg(long)]
    verbose: bool,
    #[command(subcommand)]
    shared_command: Option<SharedCommand>,
}

#[derive(Subcommand)]
enum SharedCommand {
    /// Inspect shared state.
    Inspect,
}

#[derive(Subcommand)]
enum Command {
    /// Run the demo.
    Run(RunArgs),
    /// List demo entries.
    List,
}

#[derive(ClapArgs)]
pub(crate) struct RunArgs {
    /// Skip the slow path.
    #[arg(
        long,
        visible_alias = "quick"
    )]
    fast: bool,
    /// Entry name.
    name: Option<String>,
}
