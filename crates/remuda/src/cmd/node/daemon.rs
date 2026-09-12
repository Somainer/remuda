//! A process-owned runtime with revocable control transports.

use super::super::{Args, shutdown_drivers};
use crate::{
    Shutdown,
    config::{Config, SecretRef},
};
use anyhow::{Context, Result};
use remuda_node::{
    Backoff, CarrierKind, DaemonControl, DevNode, DevServerConfig, HostInventoryConfig, NodeHello,
    ServeConfig, StdioOptions, WssConfig, WssLink, compose,
};
use std::time::Duration;
use tokio::sync::watch;

pub(super) async fn run(config: Config, args: Args, mut shutdown: Shutdown) -> Result<()> {
    let listener = remuda_node::bind_daemon(&config.data_dir).await?;
    let mut native = remuda_node::NativeDriverConfig::new(config.data_dir.clone());
    native.herdr_orphan_sweep &= !args.no_herdr_orphan_sweep;
    native.herdr_socket_dir = Some(config.data_dir.join("herdr"));
    std::fs::create_dir_all(&config.node.workspace)?;
    let runtime = compose(&ServeConfig {
        http: DevServerConfig::loopback(0).with_workspace_root(config.node.workspace.clone()),
        data_dir: config.data_dir.clone(),
        drivers: remuda_node::LocalDrivers::Native(native),
    })?;
    runtime.reconcile_herdr().await?;
    let opts = StdioOptions {
        labels: config.node.labels.clone(),
        max_instances: config.node.max_instances,
        display_label: args.display_label.clone(),
        transport: "ssh-stdio".into(),
        data_dir: config.data_dir.clone(),
    };
    let control = DaemonControl::new()?;
    let (stop, stopped) = watch::channel(false);
    let outbound_config = config.clone();
    let outbound_runtime = runtime.clone();
    let outbound_control = control.clone();
    let outbound = tokio::spawn(async move {
        let result = outbound_loop(
            outbound_config,
            args,
            outbound_runtime,
            outbound_control,
            stopped,
        )
        .await;
        if let Err(error) = &result {
            tracing::error!(%error, "outbound worker stopped; local daemon controller remains available");
        }
        result
    });
    let result = tokio::select! {
        result = remuda_node::run_daemon_runtime_listener(runtime.clone(), opts, control, &listener) => result.map_err(Into::into),
        result = shutdown.wait() => result,
    };
    stop.send_replace(true);
    let outbound_result = outbound.await.context("outbound Node worker failed");
    shutdown_drivers(&runtime, config.shutdown_timeout()).await?;
    result?;
    outbound_result?
}

async fn outbound_loop(
    config: Config,
    args: Args,
    runtime: DevNode,
    control: DaemonControl,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let Some(url) = config.node.hub_url.as_ref() else {
        let _ = stop.changed().await;
        return Ok(());
    };
    let inventory = HostInventoryConfig {
        labels: config.node.labels.clone(),
        max_instances: config.node.max_instances,
        herdr_socket: config.node.herdr_socket.clone(),
    };
    let probe = tokio::task::spawn_blocking(move || {
        NodeHello::detect_for_carrier(inventory, CarrierKind::OutboundWss)
    });
    let mut hello = tokio::select! {
        result = probe => result.context("outbound Node inventory worker failed")??,
        _ = stop.changed() => return Ok(()),
    };
    hello.params.host.host_id = runtime.host().meta.id;
    let token_path = config.data_dir.join("node/host-token");
    let enroll_path = config.data_dir.join("node/enroll-token");
    let backoff = Backoff::default();
    let mut attempt = 0_u32;
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let lease = tokio::select! {
            result = control.acquire_outbound() => result?,
            _ = stop.changed() => return Ok(()),
        };
        // A crash after the host-token rename but before enroll-token removal
        // must reconnect with the durable host credential, never the spent token.
        let token = if token_path.is_file() {
            SecretRef::File(token_path.clone()).resolve()
        } else if enroll_path.is_file() {
            SecretRef::File(enroll_path.clone()).resolve()
        } else {
            config
                .node
                .host_token
                .clone()
                .unwrap_or(SecretRef::File(token_path.clone()))
                .resolve()
        };
        let connected = match token {
            Ok(token) => {
                let wss = WssConfig {
                    url: url.clone(),
                    token: token.into_string(),
                    host_id: runtime.host().meta.id.as_id().as_str().to_owned(),
                    label: args
                        .display_label
                        .clone()
                        .unwrap_or_else(|| hello.params.host.hostname.clone()),
                    node_version: env!("CARGO_PKG_VERSION").into(),
                    cli: serde_json::to_value(&hello.params.host.cli)?,
                    host: Some(serde_json::to_value(&hello.params.host)?),
                    heartbeat_interval: Duration::from_secs(15),
                    backoff,
                    journal_queue: 32,
                };
                tokio::select! {
                    result = WssLink::connect_runtime_controlled_persisting(wss, runtime.clone(), lease.clone(), config.data_dir.clone()) => result.map_err(anyhow::Error::from),
                    _ = lease.revoked() => { continue; },
                    _ = stop.changed() => return Ok(()),
                }
            }
            Err(error) => {
                Err(error.context("daemon needs a one-shot enroll token or an enrolled host token"))
            }
        };
        match connected {
            Ok(link) => {
                if !token_path.is_file() && config.node.host_token.is_none() {
                    link.shutdown().await;
                    anyhow::bail!("Hub enrollment did not return a persistent host token");
                }
                if token_path.is_file()
                    && enroll_path.is_file()
                    && let Err(error) = std::fs::remove_file(&enroll_path)
                {
                    tracing::warn!(%error, "host token is durable but spent enrollment file could not be removed");
                }
                attempt = 0;
                tracing::info!(host_id = %link.host_id, "persistent Node connected to Hub");
                tokio::select! {
                    _ = lease.revoked() => {},
                    _ = stop.changed() => {},
                }
                link.shutdown().await;
            }
            Err(error) => {
                tracing::warn!(%error, "outbound Hub connection unavailable; daemon remains alive");
                drop(lease);
                tokio::select! {
                    _ = tokio::time::sleep(backoff.jittered_delay(attempt, u64::from(std::process::id()))) => {},
                    _ = stop.changed() => return Ok(()),
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}
