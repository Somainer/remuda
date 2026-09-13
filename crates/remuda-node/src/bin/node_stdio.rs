//! Minimal `remuda node --stdio` / `remuda version` binary for SSH bootstrap.
//!
//! Uploaded as `remuda` on the remote host. Speaks Hub NDJSON and dispatches
//! `instance.*` through the native driver registry (`generic-pty` included).

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use remuda_node::{
    DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, StdioOptions, compose,
    run_stdio_with_node,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
#[command(name = "remuda", version, about = "Remuda Node stdio carrier")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the Node over a development stdio carrier or daemon bridge.
    Node(NodeArgs),
    /// Print crate version.
    Version,
}

#[derive(Args)]
struct NodeArgs {
    #[command(subcommand)]
    action: Option<NodeAction>,
    /// Ephemeral development carrier; peer loss stops its instances.
    #[arg(long)]
    stdio: bool,
    #[arg(long = "label", global = true)]
    labels: Vec<String>,
    #[arg(long, default_value_t = 8, global = true)]
    max_instances: usize,
    #[arg(long, global = true)]
    display_label: Option<String>,
    #[arg(long, default_value = "ssh-stdio", global = true)]
    transport: String,
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum NodeAction {
    /// Persistent runtime process.
    Daemon {
        #[arg(long, hide = true)]
        detached: bool,
    },
    /// Start the persistent runtime in a new session.
    Run {
        #[arg(long)]
        daemon: bool,
    },
    /// Read-only local daemon probe.
    Status,
    /// Forward stdio to a persistent runtime.
    Bridge {
        #[arg(long)]
        no_start: bool,
    },
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
        Command::Node(args) => {
            if matches!(args.action, Some(NodeAction::Daemon { detached: true })) {
                nix::unistd::setsid()?;
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(run_node(args));
            runtime.shutdown_timeout(std::time::Duration::from_secs(1));
            result
        }
    }
}

async fn start_daemon(args: &NodeArgs, data_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if remuda_node::daemon_is_running(data_dir).await? {
        return Ok(());
    }
    std::fs::create_dir_all(data_dir)?;
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(data_dir.join("daemon.log"))?;
    let mut command = Process::new("sh");
    command
        .arg("-c")
        .arg("trap '' HUP; exec \"$@\"")
        .arg("remuda-daemon")
        .arg(std::env::current_exe()?)
        .args(["node", "daemon", "--detached", "--data-dir"])
        .arg(data_dir)
        .args(["--max-instances", &args.max_instances.to_string()]);
    if let Some(label) = &args.display_label {
        command.arg("--display-label").arg(label);
    }
    for label in &args.labels {
        command.arg("--label").arg(label);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    for _ in 0..100 {
        if remuda_node::daemon_is_running(data_dir)
            .await
            .unwrap_or(false)
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("daemon exited: {status}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("daemon startup timed out")
}

async fn run_node(args: NodeArgs) -> anyhow::Result<()> {
    let data_dir = args
        .data_dir
        .clone()
        .unwrap_or_else(|| StdioOptions::default().data_dir);
    match &args.action {
        Some(NodeAction::Status) => {
            let running = remuda_node::daemon_is_running(&data_dir).await?;
            println!("{}", serde_json::json!({"running": running}));
            anyhow::ensure!(running, "daemon is not running");
            return Ok(());
        }
        Some(NodeAction::Run { daemon }) => {
            anyhow::ensure!(*daemon, "run requires --daemon");
            return start_daemon(&args, &data_dir).await;
        }
        Some(NodeAction::Bridge { no_start }) => {
            if !*no_start {
                start_daemon(&args, &data_dir).await?;
            }
            let stream = remuda_node::connect_daemon_bridge(&data_dir, true).await?;
            let (mut read, mut write) = stream.into_split();
            let mut input = tokio::io::stdin();
            let mut output = tokio::io::stdout();
            tokio::select! {
                result = tokio::io::copy(&mut input, &mut write) => { result?; write.shutdown().await?; }
                result = tokio::io::copy(&mut read, &mut output) => { result?; output.flush().await?; }
            }
            return Ok(());
        }
        _ => {}
    }
    let daemon = matches!(args.action, Some(NodeAction::Daemon { .. }));
    anyhow::ensure!(
        daemon || args.stdio,
        "node requires --stdio or a daemon subcommand"
    );
    let listener = if daemon {
        Some(remuda_node::bind_daemon(&data_dir).await?)
    } else {
        None
    };
    let labels = parse_labels(&args.labels)?;
    remuda_node::prepare_workspace(&data_dir.join("workspace"))?;
    std::fs::create_dir_all(data_dir.join("herdr"))?;
    let mut native = NativeDriverConfig::new(data_dir.clone());
    native.herdr_socket_dir = Some(data_dir.join("herdr"));
    let node = compose(&ServeConfig {
        http: DevServerConfig::loopback(0).with_workspace_root(data_dir.join("workspace")),
        data_dir: data_dir.clone(),
        drivers: LocalDrivers::Native(native),
    })?;
    let opts = StdioOptions {
        labels,
        max_instances: args.max_instances,
        display_label: args.display_label,
        transport: args.transport,
        data_dir,
    };
    if !daemon {
        return run_stdio_with_node(node, opts).await.map_err(Into::into);
    }
    node.reconcile_herdr().await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = remuda_node::run_daemon_runtime_listener(node.clone(), opts, remuda_node::DaemonControl::new()?, listener.as_ref().context("daemon listener missing")?) => result.map_err(Into::into),
        _ = terminate.recv() => Ok(()),
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
    };
    node.shutdown().await?;
    result
}
