//! Remuda composition root. Operational command modules remain independently owned.

mod build_info;
mod cmd;
mod config;
#[path = "cmd/dev.rs"]
mod dev;
#[path = "cmd/dispatcher.rs"]
mod dispatcher;
#[path = "cmd/hub.rs"]
mod hub;
#[path = "cmd/node.rs"]
mod node;

use anyhow::{Context, ensure};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "remuda", version, about = "Unified remote agent runtime")]
struct Cli {
    /// TOML configuration; defaults to REMUDA_CONFIG or ./remuda.toml when present.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the configured data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the Hub, authentication store, and embedded Web application.
    Hub(hub::Args),
    /// Run a Node over an outbound WSS or SSH-friendly stdio carrier.
    Node(node::Args),
    /// Run a loopback Hub and local Node with a shared development access code.
    Dev(dev::Args),
    /// Run the Feishu dispatcher against a Hub using a dedicated lark-cli app.
    Dispatcher(dispatcher::Args),
    /// Print build identity without loading configuration or starting services.
    Version {
        /// Emit exactly one JSON object on stdout.
        #[arg(long)]
        json: bool,
    },
    /// SSH remote hosts: list, probe, bootstrap, and stdio node.
    Ssh(cmd::ssh::SshArgs),
    /// Create, list, send, wait, read, keys, stop, or rm a Hub-backed instance.
    Instance {
        #[command(flatten)]
        hub: cmd::hub_client::HubOpts,
        #[command(subcommand)]
        command: cmd::instance::InstanceCommand,
    },
    /// Run a spec on many hosts, or broadcast a prompt (`fleet send`).
    Fleet {
        #[command(flatten)]
        hub: cmd::hub_client::HubOpts,
        #[command(subcommand)]
        command: cmd::fleet::FleetCommand,
    },
    /// Create a git worktree (`git worktree add -b wt/<name>/…`).
    Worktree {
        #[command(subcommand)]
        command: cmd::worktree::WorktreeCommand,
    },
    /// stdio MCP server (JSON-RPC 2.0) for the same instance/fleet/worktree tools.
    Mcp {
        #[command(flatten)]
        hub: cmd::hub_client::HubOpts,
    },
}

impl Cli {
    fn load_config(&self) -> anyhow::Result<config::Config> {
        let mut config = config::Config::load(self.config.as_deref())?;
        if let Some(path) = &self.data_dir {
            ensure!(!path.as_os_str().is_empty(), "--data-dir must not be empty");
            config.data_dir = path.clone();
        }
        Ok(config)
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Command::Version { json } = cli.command {
        return build_info::write(json, &mut std::io::stdout().lock());
    }
    init_tracing()?;
    if matches!(
        &cli.command,
        Command::Hub(_) | Command::Node(_) | Command::Dev(_) | Command::Dispatcher(_)
    ) {
        let config = cli.load_config()?;
        let timeout = config.shutdown_timeout();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("cannot create service runtime")?;
        let result = runtime.block_on(async move {
            let shutdown = Shutdown::install()?;
            match cli.command {
                Command::Hub(args) => hub::run(config, args, shutdown).await,
                Command::Node(args) => node::run(config, args, shutdown).await,
                Command::Dev(args) => dev::run(config, args, shutdown).await,
                Command::Dispatcher(args) => dispatcher::run(config, args, shutdown).await,
                _ => unreachable!("only service commands enter the service runtime"),
            }
        });
        // Tokio stdin uses a blocking reader that cannot be cancelled on signal.
        // Drivers have already drained; bound the remaining runtime teardown.
        runtime.shutdown_timeout(timeout);
        return result;
    }
    // These modules own synchronous entry points that create their own runtimes.
    match cli.command {
        Command::Ssh(args) => cmd::ssh::run_blocking(args),
        Command::Instance { hub, command } => cmd::instance::run(hub, command),
        Command::Fleet { hub, command } => cmd::fleet::run(hub, command),
        Command::Worktree { command } => cmd::worktree::run(command),
        Command::Mcp { hub } => cmd::mcp::run(hub),
        _ => unreachable!("version and service commands are handled above"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn version_json_uses_the_cli_dispatch_and_contains_build_identity() {
        let cli = Cli::try_parse_from([
            "remuda",
            "--config",
            "/missing/remuda.toml",
            "version",
            "--json",
        ])
        .expect("version CLI");
        let Command::Version { json } = cli.command else {
            panic!("version command")
        };
        let mut bytes = Vec::new();
        build_info::write(json, &mut bytes).expect("version output");
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("one JSON object");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["git_sha"], env!("REMUDA_GIT_SHA"));
        assert_eq!(value["build_date"], env!("REMUDA_BUILD_DATE"));
        assert_eq!(value["target"], env!("REMUDA_TARGET"));
    }

    #[test]
    fn declares_modes_and_accepts_global_flags_after_subcommands() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "remuda",
            "hub",
            "--config",
            "test.toml",
            "--data-dir",
            "cli-data",
            "--listen",
            "127.0.0.1:1234",
        ])
        .expect("hub flags");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("test.toml"))
        );
        assert_eq!(
            cli.data_dir.as_deref(),
            Some(std::path::Path::new("cli-data"))
        );
        let Command::Hub(args) = cli.command else {
            panic!("hub command")
        };
        let mut config = config::Config::default();
        config.hub.listen.set_port(4567);
        args.apply(&mut config);
        assert_eq!(config.hub.listen.port(), 1234);
        assert!(
            Cli::try_parse_from([
                "remuda",
                "node",
                "--stdio",
                "--hub-url",
                "wss://host/v1/node"
            ])
            .is_err()
        );
        let names: Vec<_> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        assert!(names.contains(&"instance".to_string()));
        assert!(names.contains(&"fleet".to_string()));
        assert!(names.contains(&"worktree".to_string()));
        assert!(names.contains(&"mcp".to_string()));
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
        assert!(names.iter().any(|n| n == "worktree"));
        assert!(names.iter().any(|n| n == "name"));
        assert!(names.iter().any(|n| n == "cwd"));
    }

    #[test]
    fn instance_wait_declares_until_and_timeout() {
        let instance = Cli::command()
            .find_subcommand("instance")
            .expect("instance")
            .clone();
        let wait = instance.find_subcommand("wait").expect("wait");
        let names: Vec<_> = wait
            .get_arguments()
            .map(|a| a.get_id().as_str().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "until"));
        assert!(names.iter().any(|n| n == "timeout"));
        assert!(instance.find_subcommand("list").is_some());
        assert!(instance.find_subcommand("keys").is_some());
        assert!(instance.find_subcommand("rm").is_some());
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
        assert!(fleet.find_subcommand("send").is_some());
    }
}
