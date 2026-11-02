//! Fixture demo command.

use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
#[command(about = "Exercise the docs-contract fixture.")]
pub(crate) struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Print more detail.
    #[arg(long)]
    verbose: bool,
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
