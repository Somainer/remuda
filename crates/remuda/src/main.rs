//! Remuda Hub, Node, and local development command entry points.

use clap::{Parser, Subcommand};

/// Remuda process selection.
#[derive(Parser)]
#[command(name = "remuda", version, about = "Unified remote agent runtime")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Bootstrap process modes; these do not launch services yet.
#[derive(Subcommand)]
enum Command {
    /// Run the central Hub (bootstrap placeholder).
    Hub,
    /// Run an execution Node (bootstrap placeholder).
    Node,
    /// Run the local development environment (bootstrap placeholder).
    Dev,
}

fn main() {
    let mode = match Cli::parse().command {
        Command::Hub => "hub",
        Command::Node => "node",
        Command::Dev => "dev",
    };
    println!("remuda {mode}: bootstrap placeholder; no service started");
}
