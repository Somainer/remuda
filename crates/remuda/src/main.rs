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
    /// Create, send, wait, read, or stop a Hub-backed instance.
    Instance {
        #[command(flatten)]
        hub: cmd::hub_client::HubOpts,
        #[command(subcommand)]
        command: cmd::instance::InstanceCommand,
    },
    /// Run a spec on many hosts (Hub fleet HTTP).
    Fleet {
        #[command(flatten)]
        hub: cmd::hub_client::HubOpts,
        #[command(subcommand)]
        command: cmd::fleet::FleetCommand,
    },
    /// stdio MCP server (JSON-RPC 2.0) for the same instance/fleet tools.
    Mcp {
        #[command(flatten)]
        hub: cmd::hub_client::HubOpts,
    },
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
        Command::Instance { hub, command } => {
            if let Err(error) = cmd::instance::run(hub, command) {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
        Command::Fleet { hub, command } => {
            if let Err(error) = cmd::fleet::run(hub, command) {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
        Command::Mcp { hub } => {
            if let Err(error) = cmd::mcp::run(hub) {
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
        assert!(names.contains(&"instance".to_string()));
        assert!(names.contains(&"fleet".to_string()));
        assert!(names.contains(&"mcp".to_string()));
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

    #[test]
    fn instance_create_declares_host_and_labels() {
        let cli = Cli::try_parse_from([
            "remuda", "instance", "create", "--host", "hst_1", "--prompt", "hi",
        ])
        .expect("instance create");
        let Command::Instance { command, .. } = cli.command else {
            panic!("instance command")
        };
        let cmd::instance::InstanceCommand::Create { host, labels, .. } = command else {
            panic!("create")
        };
        assert_eq!(host.as_deref(), Some("hst_1"));
        assert!(labels.is_empty());
        let instance = Cli::command()
            .find_subcommand("instance")
            .expect("instance")
            .clone();
        let create = instance.find_subcommand("create").expect("create");
        let names: Vec<_> = create
            .get_arguments()
            .map(|a| a.get_id().as_str().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "host"));
        assert!(names.iter().any(|n| n == "labels"));
    }

    #[test]
    fn fleet_run_declares_hosts_and_labels() {
        let fleet = Cli::command()
            .find_subcommand("fleet")
            .expect("fleet")
            .clone();
        let run = fleet.find_subcommand("run").expect("run");
        let names: Vec<_> = run
            .get_arguments()
            .map(|a| a.get_id().as_str().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "hosts"));
        assert!(names.iter().any(|n| n == "labels"));
    }
}
