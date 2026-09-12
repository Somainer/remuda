//! Minimal `remuda node --stdio` / `remuda version` binary for SSH bootstrap.
//!
//! Uploaded as `remuda` on the remote host. Speaks Hub NDJSON and dispatches
//! `instance.*` through the native driver registry (`generic-pty` included).

use clap::{Parser, Subcommand};
use remuda_node::{
    DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, StdioOptions, compose,
    run_stdio_with_node,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "remuda", version, about = "Remuda Node stdio carrier")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Speak Hub NDJSON on stdin/stdout.
    Node {
        /// Enable the stdio carrier (required).
        #[arg(long)]
        stdio: bool,
        /// Placement label `KEY=VALUE`; repeatable.
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        /// Advertised instance ceiling.
        #[arg(long, default_value_t = 8)]
        max_instances: usize,
        /// Registry display name (`hosts[].label`).
        #[arg(long)]
        display_label: Option<String>,
        /// Carrier reported to Hub.
        #[arg(long, default_value = "ssh-stdio")]
        transport: String,
        /// Data directory for `enrollment.json`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Print crate version.
    Version,
}

fn parse_labels(raw: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for item in raw {
        let (key, value) = item
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("label must be KEY=VALUE: {item}"))?;
        anyhow::ensure!(!key.is_empty() && !value.is_empty(), "empty label {item}");
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!("remuda {}", env!("CARGO_PKG_VERSION"));
            println!("target=x86_64-unknown-linux-musl");
            println!("mode=node-stdio");
            Ok(())
        }
        Command::Node {
            stdio,
            labels,
            max_instances,
            display_label,
            transport,
            data_dir,
        } => {
            anyhow::ensure!(stdio, "remuda node requires --stdio on this binary");
            let labels = parse_labels(&labels)?;
            let data_dir = data_dir.unwrap_or_else(|| StdioOptions::default().data_dir);
            std::fs::create_dir_all(data_dir.join("workspace"))?;
            std::fs::create_dir_all(data_dir.join("herdr"))?;
            let mut native = NativeDriverConfig::new(data_dir.clone());
            native.herdr_socket_dir = Some(data_dir.join("herdr"));
            native.herdr_session = "remuda-dogfood".into();
            // Isolated CLAUDE_CONFIG_DIR copies of ~/.claude.json are rejected
            // by this host's Claude (API Usage Billing). Leave login unset so
            // the pane inherits the user's existing oauth. Herdr stays under
            // data_dir/herdr.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let node = compose(&ServeConfig {
                    http: DevServerConfig::loopback(0)
                        .with_workspace_root(data_dir.join("workspace")),
                    data_dir: data_dir.clone(),
                    drivers: LocalDrivers::Native(native),
                })?;
                run_stdio_with_node(
                    node,
                    StdioOptions {
                        labels,
                        max_instances,
                        display_label,
                        transport,
                        data_dir,
                    },
                )
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))
            })
        }
    }
}
