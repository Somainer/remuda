//! Remuda Hub, Node, and local development command entry points.

use clap::{Parser, Subcommand};

mod cmd;

/// Hub–Node wire protocol major; matches `remuda_protocol::PROTOCOL_VERSION.major`.
const WIRE_MAJOR: u16 = 1;
/// SQLite schema major. `0` until the first Hub/Node migration ships.
const SCHEMA_MAJOR: u16 = 0;

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
    /// Print semver, commit, rustc, target, and wire/schema majors.
    Version,
    /// SSH remote hosts: list, probe, bootstrap, and stdio node.
    Ssh(cmd::ssh::SshArgs),
}

fn version_text() -> String {
    format!(
        "remuda {semver}\ncommit={commit}\nrustc={rustc}\ntarget={target}\nwire={wire}\nschema={schema}\n",
        semver = env!("CARGO_PKG_VERSION"),
        commit = env!("REMUDA_GIT_SHA"),
        rustc = env!("REMUDA_RUSTC_VERSION"),
        target = env!("REMUDA_TARGET"),
        wire = WIRE_MAJOR,
        schema = SCHEMA_MAJOR,
    )
}

fn main() {
    match Cli::parse().command {
        Command::Hub => {
            println!("remuda hub: bootstrap placeholder; no service started");
        }
        Command::Node => {
            println!("remuda node: bootstrap placeholder; no service started");
        }
        Command::Dev => {
            println!("remuda dev: bootstrap placeholder; no service started");
        }
        Command::Version => {
            print!("{}", version_text());
        }
        Command::Ssh(args) => {
            if let Err(error) = cmd::ssh::run_blocking(args) {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn declares_version_and_dev_subcommands() {
        let names: Vec<_> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        assert!(names.contains(&"version".to_string()));
        assert!(names.contains(&"dev".to_string()));
        assert!(names.contains(&"hub".to_string()));
        assert!(names.contains(&"node".to_string()));
        assert!(names.contains(&"ssh".to_string()));
    }

    #[test]
    fn version_text_includes_identity_fields() {
        let text = version_text();
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains("commit="));
        assert!(text.contains("rustc="));
        assert!(text.contains("target="));
        assert!(text.contains("wire=1"));
        assert!(text.contains("schema=0"));
    }
}
