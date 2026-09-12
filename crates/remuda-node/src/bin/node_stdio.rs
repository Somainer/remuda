//! Minimal `remuda node --stdio` / `remuda version` binary for SSH bootstrap.
//!
//! The composition-root `remuda` binary currently fails to build against the
//! split Node crate. This bin is uploaded as `remuda` on the remote host.

use clap::{Parser, Subcommand};
use remuda_node::StdioOptions;
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
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                remuda_node::run_stdio_opts(StdioOptions {
                    labels,
                    max_instances,
                    display_label,
                    transport,
                    data_dir: data_dir.unwrap_or_else(|| StdioOptions::default().data_dir),
                })
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))
            })
        }
    }
}
