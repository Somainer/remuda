//! Loopback Hub + local native Claude Node, sharing one development access code.

use crate::{
    Shutdown,
    config::{Config, SecretRef},
    hub, node,
};
use anyhow::{Context, ensure};
use clap::Args as ClapArgs;
use remuda_node::{
    DevNode, DevServerConfig, MemoryStore, NativeDriverConfig, WssConfig, WssLink,
    apply_hello_result, dev_router, load_or_create_enrollment, native_driver_registry,
};
use remuda_protocol::HostId;
use serde_json::json;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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
    let (host_id, node_token) =
        load_dev_enrollment(&config.data_dir, &running_hub.bootstrap_token)?;
    let node = DevNode::with_parts_on_host(&node_config, store, drivers, host_id.clone())?;
    let mut wss = WssConfig::loopback(running_hub.addr, node_token, host_id.as_id().to_string())
        .with_collected_inventory();
    wss.label = "local-development".into();
    let link = WssLink::connect_runtime(wss, node.clone())
        .await
        .context("local Node could not enroll with the development Hub")?;
    persist_dev_enrollment(&config.data_dir, &host_id, link.node_token.as_deref())?;
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
    let (reason, server_finished) = tokio::select! {
        result = shutdown.wait() => (result, false),
        result = &mut server => {
            let result = result.context("local Node server task failed").and_then(|result| result.map_err(Into::into));
            (result.and_then(|()| Err(anyhow::anyhow!("local Node server exited before shutdown"))), true)
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

fn load_dev_enrollment(data_dir: &Path, bootstrap: &str) -> anyhow::Result<(HostId, String)> {
    let enrollment = load_or_create_enrollment(data_dir).context("node enrollment")?;
    let token = enrollment
        .node_token
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| bootstrap.to_owned());
    Ok((enrollment.host_id, token))
}

fn persist_dev_enrollment(
    data_dir: &Path,
    host_id: &HostId,
    node_token: Option<&str>,
) -> anyhow::Result<()> {
    apply_hello_result(
        data_dir,
        &json!({
            "hostId": host_id,
            "nodeToken": node_token,
        }),
    )
    .context("persist node enrollment")?;
    Ok(())
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

    #[test]
    fn development_enrollment_reuses_the_same_host_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (first, token) =
            load_dev_enrollment(dir.path(), "bootstrap-secret").expect("first enroll");
        assert_eq!(token, "bootstrap-secret");
        persist_dev_enrollment(dir.path(), &first, Some("host-secret")).expect("persist token");
        let (second, token) =
            load_dev_enrollment(dir.path(), "bootstrap-secret").expect("second enroll");
        assert_eq!(first, second);
        assert_eq!(token, "host-secret");
    }
}
