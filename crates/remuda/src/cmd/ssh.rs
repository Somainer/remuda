//! `remuda ssh` — list, probe, bootstrap, and stdio node over OpenSSH.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use remuda_ssh::{
    BootstrapResult, DEFAULT_REMOTE_BIN, NodeTransport, SshClient, SshOptions, SshTarget,
    StdioTransport, bootstrap, default_local_musl, list_user_hosts, node_stdio_argv, probe,
};
use serde_json::json;

/// SSH remote transport.
#[derive(Debug, Args)]
pub struct SshArgs {
    #[command(subcommand)]
    command: SshCommand,
}

#[derive(Debug, Subcommand)]
enum SshCommand {
    /// List `Host` aliases in `~/.ssh/config` (wildcards omitted).
    List,
    /// Resolve `ssh -G` and probe uname/glibc/remuda/claude/herdr.
    Probe {
        /// SSH config Host alias.
        alias: String,
        /// Disable ControlMaster multiplexing.
        #[arg(long)]
        no_control_master: bool,
    },
    /// Push a musl `remuda` binary, verify sha256, run `version`.
    Bootstrap {
        /// SSH config Host alias.
        alias: String,
        /// Local musl binary (default `target/x86_64-unknown-linux-musl/release/remuda`).
        #[arg(long)]
        local: Option<PathBuf>,
        /// Remote dest (default `~/.local/bin/remuda`).
        #[arg(long, default_value = DEFAULT_REMOTE_BIN)]
        remote: String,
        /// Disable ControlMaster multiplexing.
        #[arg(long)]
        no_control_master: bool,
    },
    /// Start `node --stdio` over SSH and print hello (falls back to `version`).
    Node {
        /// SSH config Host alias.
        alias: String,
        /// Remote remuda path (default `~/.local/bin/remuda`).
        #[arg(long, default_value = DEFAULT_REMOTE_BIN)]
        remote: String,
        /// Disable ControlMaster multiplexing.
        #[arg(long)]
        no_control_master: bool,
    },
}

/// Run a `remuda ssh` subcommand on the current Tokio runtime.
pub async fn run(args: SshArgs) -> Result<()> {
    match args.command {
        SshCommand::List => {
            for alias in list_user_hosts()? {
                println!("{alias}");
            }
            Ok(())
        }
        SshCommand::Probe {
            alias,
            no_control_master,
        } => {
            let client = client(&alias, !no_control_master);
            let target = SshTarget::resolve(&alias)?;
            let report = probe(&client, target).await?;
            print!("{}", report.display_text());
            client.control_exit().await?;
            Ok(())
        }
        SshCommand::Bootstrap {
            alias,
            local,
            remote,
            no_control_master,
        } => {
            let local = resolve_local(local)?;
            let client = client(&alias, !no_control_master);
            let result = bootstrap(&client, &local, &remote).await?;
            match result {
                BootstrapResult::Skipped {
                    digest,
                    remote_path,
                    version,
                } => {
                    println!("skipped digest={digest} path={remote_path}");
                    print!("{version}");
                }
                BootstrapResult::Uploaded {
                    digest,
                    remote_path,
                    version,
                } => {
                    println!("uploaded digest={digest} path={remote_path}");
                    print!("{version}");
                }
            }
            client.control_exit().await?;
            Ok(())
        }
        SshCommand::Node {
            alias,
            remote,
            no_control_master,
        } => {
            let client = client(&alias, !no_control_master);
            let target = SshTarget::resolve(&alias)?;
            let argv = node_stdio_argv(std::path::Path::new(&remote));
            match tokio::time::timeout(
                Duration::from_secs(8),
                try_stdio_hello(client.clone(), argv),
            )
            .await
            {
                Ok(Ok(value)) => {
                    println!("{value}");
                    client.control_exit().await?;
                    return Ok(());
                }
                Ok(Err(err)) => {
                    tracing::info!(error = %err, "node --stdio unavailable; falling back to version");
                }
                Err(_) => {
                    tracing::info!("node --stdio timed out; falling back to version");
                }
            }
            let version = client
                .exec(&[remote.as_str(), "version"], None, Duration::from_secs(20))
                .await;
            let version_text = match version {
                Ok(output) if output.status == Some(0) => output.stdout,
                Ok(output) => output.stderr,
                Err(err) => err.to_string(),
            };
            println!(
                "{}",
                json!({
                    "ok": true,
                    "carrier": "ssh-stdio",
                    "mode": "version-fallback",
                    "reason": "remote remuda did not emit node.hello on --stdio; see crates/remuda-ssh/README.md",
                    "alias": target.alias,
                    "version": version_text,
                })
            );
            client.control_exit().await?;
            Ok(())
        }
    }
}

/// Run `remuda ssh` by creating a Tokio runtime (sync CLI entry).
pub fn run_blocking(args: SshArgs) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?
        .block_on(run(args))
}

async fn try_stdio_hello(client: SshClient, argv: Vec<String>) -> Result<serde_json::Value> {
    let mut transport = StdioTransport::connect_ssh(client, argv).await?;
    let frame = transport
        .recv_json()
        .await?
        .context("stdio closed before hello")?;
    let _ = transport.close().await;
    let method = frame.get("method").and_then(serde_json::Value::as_str);
    if method == Some("node.hello")
        || method == Some("runtime.hello")
        || frame.get("jsonrpc").is_some()
    {
        Ok(frame)
    } else {
        bail!("first stdio frame was not node.hello: {frame}");
    }
}

fn client(alias: &str, control_master: bool) -> SshClient {
    let options = if control_master {
        SshOptions::with_control_master()
    } else {
        SshOptions::keepalive()
    };
    SshClient::new(alias, options)
}

fn resolve_local(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        anyhow::ensure!(path.is_file(), "local binary not found: {}", path.display());
        return Ok(path);
    }
    let cwd = std::env::current_dir().context("cwd")?;
    default_local_musl(&cwd).with_context(|| {
        format!(
            "no musl remuda at {}/target/x86_64-unknown-linux-musl/release/remuda (run `just linux-musl`)",
            cwd.display()
        )
    })
}
