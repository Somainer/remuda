//! Persistent Node lifecycle and detached SSH bridge entry points.

#[path = "daemon.rs"]
mod daemon;
#[path = "service_units.rs"]
mod units;

use super::Args;
use crate::{
    Shutdown,
    config::{Config, SecretRef},
};
use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use units::{LAUNCHD_LABEL, SERVICE_NAME, UnitConfig};

#[derive(Subcommand)]
pub(super) enum Command {
    /// Run the durable daemon in the foreground for a user service supervisor.
    Daemon {
        /// Establish a fresh session before the async runtime starts.
        #[arg(long, hide = true)]
        detached: bool,
    },
    /// Start a detached daemon, returning once its local socket is ready.
    Run {
        #[arg(long, required = true)]
        daemon: bool,
    },
    /// Install and enable a persistent user service.
    Install(InstallArgs),
    /// Disable and remove the user service; retain the journal and identity.
    Uninstall(ManagerArgs),
    /// Print daemon socket health; exit unsuccessfully when it is unavailable.
    Status,
    /// Forward stdio to the daemon. Bridge disconnection leaves Instances alive.
    Bridge {
        /// Refuse to start an absent daemon.
        #[arg(long)]
        no_start: bool,
        /// Refuse to replace an already attached controller.
        #[arg(long)]
        no_takeover: bool,
    },
}

#[derive(ClapArgs)]
pub(super) struct InstallArgs {
    /// Hub base URL or outbound WSS endpoint.
    #[arg(long)]
    hub: Option<String>,
    /// One-shot token minted by the Hub device enrollment API (D-018).
    #[arg(long, requires = "hub")]
    enroll_token: Option<String>,
    #[command(flatten)]
    manager: ManagerArgs,
}

#[derive(ClapArgs)]
pub(super) struct ManagerArgs {
    /// Install a macOS launchd user agent.
    #[arg(long, conflicts_with = "systemd_user")]
    launchd: bool,
    /// Install a Linux systemd --user service.
    #[arg(long)]
    systemd_user: bool,
}

impl ManagerArgs {
    fn launchd(&self) -> Result<bool> {
        if self.launchd {
            ensure!(cfg!(target_os = "macos"), "--launchd requires macOS");
            Ok(true)
        } else if self.systemd_user {
            ensure!(cfg!(target_os = "linux"), "--systemd-user requires Linux");
            Ok(false)
        } else if cfg!(target_os = "macos") {
            Ok(true)
        } else {
            ensure!(
                cfg!(target_os = "linux"),
                "user services require Linux or macOS"
            );
            Ok(false)
        }
    }
}

pub(super) fn detach_session(args: &Args) -> Result<()> {
    if matches!(args.command, Some(Command::Daemon { detached: true })) {
        #[cfg(unix)]
        nix::unistd::setsid().context("cannot detach Node daemon from the control terminal")?;
        #[cfg(not(unix))]
        anyhow::bail!("detached Node daemon requires Unix");
    }
    Ok(())
}

pub(super) async fn run(
    mut config: Config,
    args: Args,
    command: Command,
    mut shutdown: Shutdown,
) -> Result<()> {
    if !config.data_dir.is_absolute() {
        config.data_dir = std::env::current_dir()?.join(&config.data_dir);
    }
    ensure!(
        !args.stdio,
        "--stdio cannot be combined with a daemon subcommand"
    );
    match command {
        Command::Daemon { .. } => daemon::run(config, args, shutdown).await,
        Command::Run { daemon } => {
            ensure!(daemon, "node run requires --daemon");
            start_detached(&config, &args).await
        }
        Command::Install(install) => install_service(config, args, install).await,
        Command::Uninstall(manager) => uninstall_service(&config, manager).await,
        Command::Status => {
            let running = remuda_node::daemon_is_running(&config.data_dir).await?;
            println!(
                "{}",
                serde_json::json!({
                    "running": running, "durable": true,
                    "socket": remuda_node::daemon_socket_path(&config.data_dir)
                })
            );
            ensure!(running, "Node daemon is not running");
            Ok(())
        }
        Command::Bridge {
            no_start,
            no_takeover,
        } => {
            if !running_or_initializing(&config.data_dir).await? {
                ensure!(
                    !no_start,
                    "Node daemon is unavailable and --no-start was supplied"
                );
                start_detached(&config, &args).await?;
            }
            #[cfg(unix)]
            {
                let stream =
                    remuda_node::connect_daemon_bridge(&config.data_dir, !no_takeover).await?;
                let (mut input, mut output) = stream.into_split();
                let (mut stdin, mut stdout) = (tokio::io::stdin(), tokio::io::stdout());
                tokio::select! {
                    result = tokio::io::copy(&mut stdin, &mut output) => { result?; },
                    result = tokio::io::copy(&mut input, &mut stdout) => { result?; },
                    result = shutdown.wait() => { result?; },
                }
                Ok(())
            }
            #[cfg(not(unix))]
            anyhow::bail!("Node bridge requires a Unix domain socket")
        }
    }
}

fn hub_endpoint(value: &str) -> String {
    let value = value.trim_end_matches('/');
    let endpoint = if let Some(rest) = value.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = value.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        value.into()
    };
    if endpoint.ends_with("/v1/node") {
        endpoint
    } else {
        format!("{endpoint}/v1/node")
    }
}

fn daemon_config(config: &Config) -> Result<PathBuf> {
    #[derive(serde::Serialize)]
    struct Persisted<'a> {
        data_dir: &'a Path,
        shutdown_timeout_secs: u64,
        node: PersistedNode<'a>,
    }
    #[derive(serde::Serialize)]
    struct PersistedNode<'a> {
        workspace: &'a Path,
        labels: &'a BTreeMap<String, String>,
        max_instances: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        hub_url: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        host_token: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        herdr_socket: Option<&'a Path>,
    }
    let reference = config
        .node
        .host_token
        .as_ref()
        .map(|reference| match reference {
            SecretRef::File(path) => format!("file:{}", path.display()),
            SecretRef::Env(name) => format!("env:{name}"),
        });
    let text = toml::to_string(&Persisted {
        data_dir: &config.data_dir,
        shutdown_timeout_secs: config.shutdown_timeout_secs,
        node: PersistedNode {
            workspace: &config.node.workspace,
            labels: &config.node.labels,
            max_instances: config.node.max_instances,
            hub_url: config.node.hub_url.as_deref(),
            host_token: reference,
            herdr_socket: config.node.herdr_socket.as_deref(),
        },
    })?;
    let path = config.data_dir.join("node/daemon.toml");
    private_write(&path, &text)?;
    Ok(path)
}

fn private_write(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().context("private file has no parent")?)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("cannot create private service file")?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(
            path.parent()
                .ok_or_else(|| std::io::Error::other("private file has no parent"))?,
        )?
        .sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("cannot persist private service file")
}

fn service_environment() -> BTreeMap<String, String> {
    [
        "PATH",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ]
    .into_iter()
    .filter_map(|key| std::env::var(key).ok().map(|value| (key.into(), value)))
    .collect()
}

async fn install_service(mut config: Config, args: Args, install: InstallArgs) -> Result<()> {
    let launchd = install.manager.launchd()?;
    if let Some(hub) = install.hub {
        config.node.hub_url = Some(hub_endpoint(&hub));
    }
    config.validate()?;
    if install.enroll_token.is_some() {
        ensure!(
            !running_or_initializing(&config.data_dir).await?,
            "stop the existing Node daemon before replacing its enrollment token"
        );
    }
    std::fs::create_dir_all(config.data_dir.join("node"))?;
    if let Some(token) = install.enroll_token {
        ensure!(
            !token.trim().is_empty(),
            "enrollment token must not be empty"
        );
        private_write(&config.data_dir.join("node/enroll-token"), token.trim())?;
        // Explicit re-enrollment replaces a previous Hub identity credential.
        let previous = config.data_dir.join("node/host-token");
        if previous.is_file() {
            std::fs::remove_file(previous)?;
        }
        config.node.host_token = None;
    }
    if let Some(reference) = &config.node.host_token {
        let token = reference.resolve()?.into_string();
        let token_path = config.data_dir.join("node/host-token");
        super::persist_host_token(&token_path, &token)?;
        config.node.host_token = Some(SecretRef::File(token_path));
    }
    let config_path = daemon_config(&config)?;
    let executable = std::env::current_exe()?;
    let environment = service_environment();
    let unit = UnitConfig {
        executable: &executable,
        config: &config_path,
        data_dir: &config.data_dir,
        display_label: args.display_label.as_deref(),
        no_orphan_sweep: args.no_herdr_orphan_sweep,
        environment: &environment,
    };
    let path = service_path(launchd)?;
    if path.exists() {
        let current = std::fs::read_to_string(&path)?;
        ensure!(
            current.contains(&units::data_dir_marker(&config.data_dir)?),
            "an existing Node user service belongs to a different data directory; use node run --daemon for this directory"
        );
    }
    private_write(
        &path,
        &if launchd {
            units::launchd_unit(&unit)?
        } else {
            units::systemd_unit(&unit)?
        },
    )?;
    if launchd {
        let domain = launchd_domain().await?;
        let _ = tokio::process::Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
            .output()
            .await;
        checked(
            "launchctl",
            &["enable", &format!("{domain}/{LAUNCHD_LABEL}")],
        )
        .await?;
        checked(
            "launchctl",
            &[
                "bootstrap",
                &domain,
                path.to_str().context("unit path must be UTF-8")?,
            ],
        )
        .await?;
    } else {
        checked("systemctl", &["--user", "daemon-reload"]).await?;
        checked("systemctl", &["--user", "enable", SERVICE_NAME]).await?;
        checked("systemctl", &["--user", "restart", SERVICE_NAME]).await?;
    }
    await_ready(&config.data_dir, None).await?;
    println!(
        "{}",
        serde_json::json!({"installed": true, "unit": path, "running": true})
    );
    Ok(())
}

fn service_path(launchd: bool) -> Result<PathBuf> {
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME is required for a user service")?);
    Ok(if launchd {
        home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user")
            .join(SERVICE_NAME)
    })
}

async fn launchd_domain() -> Result<String> {
    let output = tokio::process::Command::new("id")
        .arg("-u")
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "cannot determine launchd user domain"
    );
    let uid = String::from_utf8(output.stdout)?;
    ensure!(uid.trim().parse::<u32>().is_ok(), "invalid launchd user id");
    Ok(format!("gui/{}", uid.trim()))
}

async fn checked(program: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("cannot run {program}"))?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

async fn uninstall_service(config: &Config, manager: ManagerArgs) -> Result<()> {
    let launchd = manager.launchd()?;
    let path = service_path(launchd)?;
    if !path.exists() {
        return Ok(());
    }
    ensure!(
        std::fs::read_to_string(&path)?.contains(&units::data_dir_marker(&config.data_dir)?),
        "the installed Node service belongs to another data directory; select its --data-dir to uninstall"
    );
    let stopped = if launchd {
        let domain = launchd_domain().await?;
        checked(
            "launchctl",
            &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
        )
        .await
    } else {
        checked("systemctl", &["--user", "disable", "--now", SERVICE_NAME]).await
    };
    std::fs::remove_file(path)?;
    stopped.context(
        "user service file removed, but the service manager could not confirm daemon shutdown",
    )?;
    if !launchd {
        checked("systemctl", &["--user", "daemon-reload"]).await?;
    }
    Ok(())
}

async fn await_ready(data_dir: &Path, mut child: Option<&mut std::process::Child>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match remuda_node::daemon_is_running(data_dir).await {
                Ok(true) => return Ok::<_, anyhow::Error>(()),
                Ok(false) => {}
                Err(error) => tracing::debug!(%error, "daemon socket is not ready yet"),
            }
            if let Some(child) = child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                anyhow::bail!(
                    "Node daemon exited before readiness ({status}); inspect node/daemon.log"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("Node daemon did not become ready; inspect the user service or node/daemon.log")?
}

async fn running_or_initializing(data_dir: &Path) -> Result<bool> {
    match remuda_node::daemon_is_running(data_dir).await {
        Ok(running) => Ok(running),
        Err(error) if remuda_node::daemon_socket_path(data_dir).exists() => {
            tracing::debug!(%error, "waiting for reserved daemon socket to finish initialization");
            await_ready(data_dir, None).await?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

async fn start_detached(config: &Config, args: &Args) -> Result<()> {
    if running_or_initializing(&config.data_dir).await? {
        return Ok(());
    }
    let existing = config.data_dir.join("node/daemon.toml");
    let config_path = if existing.is_file() {
        existing
    } else {
        daemon_config(config)?
    };
    let executable = std::env::current_exe()?;
    let environment = service_environment();
    let unit = UnitConfig {
        executable: &executable,
        config: &config_path,
        data_dir: &config.data_dir,
        display_label: args.display_label.as_deref(),
        no_orphan_sweep: args.no_herdr_orphan_sweep,
        environment: &environment,
    };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options.open(config.data_dir.join("node/daemon.log"))?;
    let mut child = std::process::Command::new("sh");
    // Ignored SIGHUP survives exec. The child invokes setsid before constructing
    // Tokio, so no control terminal or SSH descriptors belong to the daemon.
    child
        .args(["-c", "trap '' HUP; exec \"$@\"", "remuda-daemon"])
        .args(unit.arguments()?)
        .arg("--detached")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = child.spawn().context("cannot spawn detached Node daemon")?;
    await_ready(&config.data_dir, Some(&mut child)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_hub_urls_use_the_outbound_endpoint() {
        assert_eq!(
            hub_endpoint("https://hub.example/"),
            "wss://hub.example/v1/node"
        );
        assert_eq!(
            hub_endpoint("wss://hub.example/v1/node"),
            "wss://hub.example/v1/node"
        );
        assert_eq!(
            hub_endpoint("http://127.0.0.1:59180"),
            "ws://127.0.0.1:59180/v1/node"
        );
    }

    #[test]
    fn daemon_snapshot_is_private_and_contains_only_token_references() {
        let fixture = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: fixture.path().to_owned(),
            ..Config::default()
        };
        config.node.host_token = Some(SecretRef::File(fixture.path().join("node/host-token")));
        let path = daemon_config(&config).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(loaded.node.host_token, config.node.host_token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_waits_for_a_status_reply_after_socket_reservation() {
        let fixture = tempfile::tempdir().unwrap();
        let data_dir = fixture.path().to_owned();
        let listener = remuda_node::bind_daemon(&data_dir).await.unwrap();
        let wait_dir = data_dir.clone();
        let ready = tokio::spawn(async move { await_ready(&wait_dir, None).await });
        // The socket exists throughout initialization but is not yet serving.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!ready.is_finished());
        let node = remuda_node::DevNode::new(&remuda_node::DevServerConfig::loopback(0)).unwrap();
        let server = tokio::spawn(async move {
            remuda_node::run_daemon_runtime_listener(
                node,
                remuda_node::StdioOptions {
                    data_dir,
                    ..Default::default()
                },
                remuda_node::DaemonControl::new().unwrap(),
                &listener,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), ready)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn daemon_subcommands_accept_remote_inventory_flags_after_the_command() {
        use clap::Parser;
        for command in [
            vec![
                "remuda",
                "node",
                "install",
                "--systemd-user",
                "--display-label",
                "<sg-host>",
                "--label",
                "region=sg",
            ],
            vec![
                "remuda",
                "node",
                "bridge",
                "--no-start",
                "--data-dir",
                "/tmp/remuda-fixture",
            ],
            vec![
                "remuda",
                "node",
                "run",
                "--daemon",
                "--data-dir",
                "/tmp/remuda-fixture",
            ],
            vec![
                "remuda",
                "node",
                "daemon",
                "--detached",
                "--no-herdr-orphan-sweep",
            ],
        ] {
            crate::Cli::try_parse_from(command).unwrap();
        }
    }
}
