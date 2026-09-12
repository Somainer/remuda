//! Loopback Hub + local native Claude Node, sharing one development access code.

use crate::{
    Shutdown,
    config::{Config, SecretRef},
    hub, node,
};
use anyhow::{Context, ensure};
use clap::Args as ClapArgs;
use remuda_node::{
    DevNode, DevServerConfig, HubRequest, JournalSender, MemoryStore, NativeDriverConfig,
    WssConfig, WssLink, dev_router, dispatch_hub_rpc, native_driver_registry,
};
use remuda_protocol::InstanceId;
use serde_json::Value;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::broadcast;

#[derive(ClapArgs)]
pub(crate) struct Args {
    /// Local Node HTTP/WebSocket listener (loopback unless --dev-bind-lan).
    #[arg(long, conflicts_with = "port")]
    listen: Option<SocketAddr>,
    /// Local Node port; retains the default configured loopback address.
    #[arg(long)]
    port: Option<u16>,
    /// Hub HTTP/WebSocket listener (loopback unless --dev-bind-lan).
    #[arg(long)]
    hub_listen: Option<SocketAddr>,
    /// Bind Hub and Node for trusted-LAN access; requires --access-code-file.
    #[arg(long, requires = "access_code_file")]
    dev_bind_lan: bool,
    /// Reuse a private access-code file (required for --dev-bind-lan).
    #[arg(long)]
    access_code_file: Option<PathBuf>,
    /// Allowed development browser origin; repeatable.
    #[arg(long = "web-origin")]
    web_origins: Vec<String>,
    /// Workspace root advertised by the local Node.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Serve Hub web assets from this directory (typically `web/dist`).
    #[arg(long)]
    web_root: Option<PathBuf>,
}

impl Args {
    fn apply(&self, config: &mut Config) -> anyhow::Result<()> {
        ensure!(
            !self.dev_bind_lan || self.access_code_file.is_some(),
            "--dev-bind-lan requires --access-code-file"
        );
        if let Some(listen) = self.listen {
            config.node.listen = listen;
        }
        if let Some(port) = self.port {
            config.node.listen.set_port(port);
        }
        if let Some(listen) = self.hub_listen {
            config.hub.listen = listen;
        }
        if let Some(path) = &self.access_code_file {
            config.hub.bootstrap_token = Some(SecretRef::File(path.clone()));
        }
        if let Some(root) = &self.workspace {
            config.node.workspace = root.clone();
        }
        if let Some(root) = &self.web_root {
            config.hub.web_root = Some(root.clone());
        }
        if !self.web_origins.is_empty() {
            config.node.web_origins = self.web_origins.clone();
        }
        if self.dev_bind_lan {
            if config.node.listen.ip().is_loopback() {
                config.node.listen.set_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            }
            if config.hub.listen.ip().is_loopback() {
                config.hub.listen.set_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            }
        } else {
            ensure!(
                config.hub.listen.ip().is_loopback() && config.node.listen.ip().is_loopback(),
                "non-loopback remuda dev listeners require --dev-bind-lan and --access-code-file"
            );
        }
        config.hub.cookie_secure = false;
        config.data_dir = config.data_dir.join("dev-hub");
        config.validate()
    }
}

pub(crate) async fn run(
    mut config: Config,
    args: Args,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    args.apply(&mut config)?;
    if let Some(path) = &args.access_code_file {
        validate_private_access_code_file(path)?;
    }
    let running_hub = hub::start(&config).await?;
    let mut origins = config.node.web_origins.clone();
    origins.push(format!("http://{}", running_hub.addr));
    origins.push(format!("http://localhost:{}", running_hub.addr.port()));
    let mut node_config = DevServerConfig::loopback(config.node.listen.port())
        .with_workspace_root(config.node.workspace.clone())
        .with_allowed_origins(origins)?
        .with_access_code(running_hub.bootstrap_token.clone())?;
    node_config.bind_addr = config.node.listen;
    let mut native = NativeDriverConfig::new(config.data_dir.join("node"));
    if let Some(binary) = resolve_claude_binary() {
        tracing::info!(path = %binary.display(), "using Claude binary from PATH");
        native = native.with_claude_binary(binary);
    }
    let drivers = native_driver_registry(native)?;
    let store = Arc::new(MemoryStore::new(node_config.follow_buffer_capacity));
    let node = DevNode::with_parts(&node_config, store, drivers)?;
    let mut wss = WssConfig::loopback(
        running_hub.addr,
        running_hub.bootstrap_token.clone(),
        node.host().meta.id.as_id().to_string(),
    );
    wss.label = "local-development".into();
    let mut link = WssLink::connect(wss)
        .await
        .context("local Node could not enroll with the development Hub")?;
    let journal = link.journal_sender();
    let listener = tokio::net::TcpListener::bind(node_config.bind_addr).await?;
    let address = listener.local_addr()?;
    let accepting = Arc::new(AtomicBool::new(true));
    let policy = accepting.clone();
    let app = dev_router(node.clone(), &node_config).layer(axum::middleware::from_fn(
        move |request, next: axum::middleware::Next| {
            let policy = policy.clone();
            async move {
                if policy.load(Ordering::Acquire) {
                    next.run(request).await
                } else {
                    use axum::response::IntoResponse;
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        },
    ));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    tracing::info!(hub = %running_hub.addr, node = %address, "remuda dev listening");
    if let Some(reference) = &config.hub.bootstrap_token {
        tracing::info!(
            ?reference,
            "development access code uses the configured secret reference"
        );
    } else {
        tracing::info!(file = %config.data_dir.join("bootstrap-token").display(), "development access code file");
    }
    let (reason, server_finished) = loop {
        tokio::select! {
            result = shutdown.wait() => break (result, false),
            result = &mut server => {
                let result = result.context("local Node server task failed").and_then(|result| result.map_err(Into::into));
                break (result.and_then(|()| Err(anyhow::anyhow!("local Node server exited before shutdown"))), true);
            }
            request = link.next_hub_request() => {
                let Some(request) = request else {
                    break (Err(anyhow::anyhow!("local Node Hub link closed before shutdown")), false);
                };
                handle_hub_request(&node, &journal, request).await;
            }
        }
    };
    accepting.store(false, Ordering::Release);
    let _ = stop.send(());
    // Finish accepted HTTP mutations before enumerating driver tasks to close.
    let mut http_result = Ok(());
    if !server_finished {
        match tokio::time::timeout(config.shutdown_timeout(), &mut server).await {
            Ok(result) => {
                http_result = result
                    .context("local Node server shutdown failed")
                    .and_then(|result| result.map_err(Into::into));
            }
            Err(_) => {
                server.abort();
                let _ = server.await;
                tracing::warn!("local HTTP drain deadline expired; connections were closed");
            }
        }
    }
    let drivers = node::shutdown_drivers(&node, config.shutdown_timeout()).await;
    tokio::time::timeout(config.shutdown_timeout(), link.shutdown())
        .await
        .ok();
    drop(running_hub);
    drivers?;
    http_result?;
    reason
}

fn resolve_claude_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("REMUDA_CLAUDE_BIN") {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return None;
        }
        return Some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("claude");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn handle_hub_request(node: &DevNode, journal: &JournalSender, request: HubRequest) {
    let method = request.method.clone();
    let params = request.params.clone();
    let result = dispatch_hub_rpc(node, &method, params).await;
    if method == "instance.create"
        && let Ok(value) = &result
        && let Some(instance_id) = create_instance_id(value)
    {
        spawn_journal_mirror(node.clone(), journal.clone(), instance_id);
    }
    if let Err(error) = request.respond(result).await {
        tracing::warn!(%error, "failed to reply to Hub request");
    }
}

fn create_instance_id(value: &Value) -> Option<String> {
    value
        .pointer("/instance/id")
        .or_else(|| value.pointer("/instance/instanceId"))
        .or_else(|| value.get("instanceId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn spawn_journal_mirror(node: DevNode, journal: JournalSender, instance_id: String) {
    tokio::spawn(async move {
        let Ok(id) = InstanceId::from_str(&instance_id) else {
            return;
        };
        let Ok(mut rx) = node.subscribe(&id) else {
            return;
        };
        let Ok(instance) = node.get_instance(&id) else {
            return;
        };
        if let Ok(page) = node.read_journal(&instance.journal_id, None, 256) {
            for event in page.events {
                let Ok(value) = serde_json::to_value(&event) else {
                    continue;
                };
                if let Err(error) = journal.append(instance_id.clone(), value).await {
                    tracing::debug!(%error, %instance_id, "hub journal backfill stopped");
                    return;
                }
            }
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let Ok(value) = serde_json::to_value(&event) else {
                        continue;
                    };
                    if let Err(error) = journal.append(instance_id.clone(), value).await {
                        tracing::debug!(%error, %instance_id, "hub journal mirror stopped");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn validate_private_access_code_file(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cannot inspect access-code file {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "access-code path must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "access-code file permissions must not grant group or other access"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_mode_rejects_non_loopback_config_and_flag_overrides() {
        let args = || Args {
            listen: None,
            port: None,
            hub_listen: None,
            dev_bind_lan: false,
            access_code_file: None,
            web_origins: Vec::new(),
            workspace: None,
            web_root: None,
        };
        let mut config = Config::default();
        config.hub.listen = "0.0.0.0:8080".parse().expect("fixture address");
        assert!(args().apply(&mut config).is_err());
        let mut config = Config::default();
        let mut flags = args();
        flags.listen = Some("0.0.0.0:8787".parse().expect("fixture address"));
        assert!(flags.apply(&mut config).is_err());
        let mut config = Config::default();
        args().apply(&mut config).expect("loopback defaults");
        assert!(!config.hub.cookie_secure);
        assert!(config.data_dir.ends_with("dev-hub"));

        let mut config = Config::default();
        let mut flags = args();
        flags.dev_bind_lan = true;
        assert!(flags.apply(&mut config).is_err());

        let mut config = Config::default();
        let mut flags = args();
        flags.dev_bind_lan = true;
        flags.access_code_file = Some(PathBuf::from("/tmp/remuda-dev-code"));
        flags.apply(&mut config).expect("explicit LAN mode");
        assert!(config.hub.listen.ip().is_unspecified());
        assert!(config.node.listen.ip().is_unspecified());
    }
}
