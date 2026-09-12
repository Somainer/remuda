//! Node carrier selection and driver shutdown through the current public Node API.

use crate::{
    Shutdown,
    config::{Config, SecretRef, parse_labels},
};
use anyhow::{Context, ensure};
use clap::Args as ClapArgs;
use remuda_node::{
    Backoff, CarrierKind, DevNode, DevServerConfig, HostInventoryConfig, NodeHello, ServeConfig,
    WssConfig, WssLink, compose, load_or_create_host_id,
};
use std::{path::PathBuf, time::Duration};

#[derive(ClapArgs)]
pub(crate) struct Args {
    /// Preserve unknown Herdr panes at startup for manual recovery.
    #[arg(long)]
    no_herdr_orphan_sweep: bool,
    /// Carry Node NDJSON over stdin/stdout; logs always use stderr.
    #[arg(long, conflicts_with = "hub_url")]
    stdio: bool,
    /// Registry display name for a supervised SSH host.
    #[arg(long)]
    display_label: Option<String>,
    /// Authenticated outbound WSS endpoint; defaults to the configured hub_url.
    #[arg(long)]
    hub_url: Option<String>,
    /// Private file containing the host enrollment token.
    #[arg(long)]
    host_token_file: Option<PathBuf>,
    /// Placement label in KEY=VALUE form; repeated labels override configured keys.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,
    /// Advertised maximum number of managed Instances.
    #[arg(long)]
    max_instances: Option<usize>,
    /// Explicit Herdr server socket included in the inventory.
    #[arg(long)]
    herdr_socket: Option<PathBuf>,
}

impl Args {
    pub(crate) fn apply(&self, config: &mut Config) -> anyhow::Result<()> {
        if let Some(url) = &self.hub_url {
            config.node.hub_url = Some(url.clone());
        }
        if let Some(path) = &self.host_token_file {
            config.node.host_token = Some(SecretRef::File(path.clone()));
        }
        if let Some(max) = self.max_instances {
            config.node.max_instances = max;
        }
        if let Some(path) = &self.herdr_socket {
            config.node.herdr_socket = Some(path.clone());
        }
        config.node.labels.extend(parse_labels(&self.labels)?);
        config.validate()
    }
}

pub(crate) async fn run(
    mut config: Config,
    args: Args,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    args.apply(&mut config)?;
    if !args.stdio {
        ensure!(
            config.node.hub_url.is_some(),
            "node requires --stdio or a configured --hub-url"
        );
    }
    if !config.provider_profiles.is_empty() {
        tracing::warn!(
            "provider profiles are validated; the current Node carrier has no profile registration API"
        );
    }
    let carrier_kind = if args.stdio {
        CarrierKind::StdioNdjson
    } else {
        CarrierKind::OutboundWss
    };
    let mut hello = NodeHello::detect_for_carrier(
        HostInventoryConfig {
            labels: config.node.labels.clone(),
            max_instances: config.node.max_instances,
            herdr_socket: config.node.herdr_socket.clone(),
        },
        carrier_kind,
    )?;
    let node_dir = config.data_dir.join("node");
    std::fs::create_dir_all(&node_dir)?;
    hello.params.host.host_id = load_or_create_host_id(&node_dir)?;
    if args.stdio {
        let mut native = remuda_node::NativeDriverConfig::new(config.data_dir.clone());
        native.herdr_orphan_sweep &= !args.no_herdr_orphan_sweep;
        native.herdr_socket_dir = Some(config.data_dir.join("herdr"));
        std::fs::create_dir_all(&config.node.workspace)?;
        let runtime = compose(&ServeConfig {
            http: DevServerConfig::loopback(0).with_workspace_root(config.node.workspace.clone()),
            data_dir: config.data_dir.clone(),
            drivers: remuda_node::LocalDrivers::Native(native),
        })?;
        let opts = remuda_node::StdioOptions {
            labels: config.node.labels.clone(),
            max_instances: config.node.max_instances,
            display_label: args.display_label,
            transport: "ssh-stdio".into(),
            data_dir: config.data_dir.clone(),
        };
        let result = tokio::select! {
            result = remuda_node::run_stdio_runtime_opts(runtime.clone(), opts) => result.map_err(Into::into),
            result = shutdown.wait() => result,
        };
        shutdown_drivers(&runtime, config.shutdown_timeout()).await?;
        return result;
    }
    let token_file = node_dir.join("host-token");
    let token_ref = config
        .node
        .host_token
        .clone()
        .unwrap_or(SecretRef::File(token_file.clone()));
    let token = token_ref
        .resolve()
        .context("node requires a host token or bootstrap secret reference")?
        .into_string();
    let http = DevServerConfig::loopback(0).with_workspace_root(config.node.workspace.clone());
    let mut service = ServeConfig::native(http, config.data_dir.clone());
    if let remuda_node::LocalDrivers::Native(native) = &mut service.drivers {
        native.herdr_orphan_sweep &= !args.no_herdr_orphan_sweep;
    }
    let runtime = compose(&service)?;
    runtime.reconcile_herdr().await?;
    ensure!(
        runtime.host().meta.id == hello.params.host.host_id,
        "persisted Node runtime and enrollment host identities diverged"
    );
    let wss = WssConfig {
        url: config.node.hub_url.clone().context("missing Hub URL")?,
        token,
        host_id: hello.params.host.host_id.as_id().as_str().to_owned(),
        label: hello.params.host.hostname.clone(),
        node_version: env!("CARGO_PKG_VERSION").to_owned(),
        cli: serde_json::to_value(&hello.params.host.cli)?,
        host: Some(serde_json::to_value(&hello.params.host)?),
        heartbeat_interval: Duration::from_secs(15),
        backoff: Backoff::default(),
        journal_queue: 32,
    };
    let link = WssLink::connect_runtime(wss, runtime.clone()).await?;
    if let Some(token) = link.node_token.as_deref() {
        persist_host_token(&token_file, token)?;
    }
    tracing::info!(host_id = %link.host_id, "Node enrolled with Hub and runtime dispatch is active");
    let result = shutdown.wait().await;
    shutdown_drivers(&runtime, config.shutdown_timeout()).await?;
    link.shutdown().await;
    result
}

/// Enroll with configured inventory and cancellable I/O. Unsupported instance
/// dispatch receives an explicit JSON-RPC error.
#[cfg(test)]
pub(crate) async fn outbound_session(
    url: &str,
    token: String,
    hello: serde_json::Value,
    token_file: Option<PathBuf>,
    shutdown_timeout: Duration,
    stop: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    use remuda_node::{NodeTransport, WssCarrier};
    use serde_json::json;
    tokio::pin!(stop);
    let mut carrier = tokio::select! {
        result = WssCarrier::connect(url, &token) => result?,
        result = &mut stop => return result,
    };
    let handshake = async {
        carrier.send_json(&json!({"jsonrpc":"2.0", "id":"remuda-hello", "method":"node.hello", "params":hello})).await?;
        let response = carrier
            .recv_json()
            .await?
            .context("Hub disconnected during enrollment")?;
        ensure!(
            response["jsonrpc"] == "2.0"
                && response["id"] == "remuda-hello"
                && response.get("error").is_none()
                && response["result"].is_object(),
            "Hub rejected enrollment or returned an invalid hello response"
        );
        if let Some(token) = response["result"]["nodeToken"].as_str()
            && let Some(path) = &token_file
        {
            persist_host_token(path, token)?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(30), handshake) => result.context("Hub enrollment deadline exceeded")??,
        result = &mut stop => return result,
    }
    tracing::info!("Node enrolled with Hub");
    tracing::warn!(
        "Hub instance dispatch is not yet exposed by the Node application; requests receive an unsupported-method error"
    );
    // TODO(remuda-node): WssLink needs configured labels/maxInstances and
    // cancellation during reconnect; the application also needs an instance
    // dispatcher. Until then use the low-level carrier and exit on disconnect
    // for supervision, without replaying commands.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let pump = async {
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if let Err(error) = carrier.send_json(&json!({"jsonrpc":"2.0", "method":"node.heartbeat", "params":{}})).await {
                        break Err(error.into());
                    }
                }
                frame = carrier.recv_json() => {
                    let frame = match frame {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break Err(anyhow::anyhow!("Hub disconnected; no command was replayed")),
                        Err(error) => break Err(error.into()),
                    };
                    if frame["jsonrpc"] != "2.0" { break Err(anyhow::anyhow!("invalid Hub JSON-RPC frame")); }
                    if frame.get("method").is_some()
                        && let Some(id) = frame.get("id").filter(|id| !id.is_null())
                    {
                        let response = json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"Node application dispatcher is not wired"}});
                        if let Err(error) = carrier.send_json(&response).await { break Err(error.into()); }
                    } else if frame.get("method").is_none() && frame.get("result").is_none() && frame.get("error").is_none() {
                        break Err(anyhow::anyhow!("Hub frame is neither a request nor a response"));
                    }
                }
            }
        }
    };
    let result = tokio::select! {
        result = &mut stop => result,
        result = pump => result,
    };
    let close = tokio::time::timeout(shutdown_timeout, carrier.close())
        .await
        .context("Node connection close deadline exceeded")?;
    result?;
    close?;
    Ok(())
}

fn persist_host_token(path: &std::path::Path, token: &str) -> anyhow::Result<()> {
    use std::io::Write;
    ensure!(!token.trim().is_empty(), "Hub returned an empty host token");
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
        .context("cannot create private host-token file")?;
    let result = (|| {
        writeln!(file, "{token}")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("cannot persist enrolled host token")
}

/// Reclaim drivers independently of whether their command worker is still alive.
pub(crate) async fn shutdown_drivers(node: &DevNode, deadline: Duration) -> anyhow::Result<()> {
    tokio::time::timeout(deadline, node.shutdown())
        .await
        .context("driver shutdown deadline exceeded; resources retained for reconciliation")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outbound_enrolls_but_rejects_unwired_dispatch_instead_of_success_ack() {
        use axum::{
            Router,
            extract::ws::{Message, WebSocketUpgrade},
            routing::get,
        };
        use serde_json::{Value, json};
        use std::sync::Arc;
        let (checked, received) = tokio::sync::oneshot::channel();
        let checked = Arc::new(tokio::sync::Mutex::new(Some(checked)));
        let app = Router::new().route("/v1/node", get(move |upgrade: WebSocketUpgrade, headers: axum::http::HeaderMap| {
            let checked = checked.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-bootstrap");
                upgrade.on_upgrade(move |mut socket| async move {
                    let frame = socket.recv().await.expect("hello frame").expect("hello read");
                    let Message::Text(frame) = frame else { panic!("JSON hello") };
                    let hello: Value = serde_json::from_str(&frame).expect("hello JSON");
                    assert_eq!(hello["method"], "node.hello");
                    socket.send(Message::Text(json!({"jsonrpc":"2.0","id":hello["id"],"result":{"ok":true}}).to_string().into())).await.expect("hello result");
                    socket.send(Message::Text(json!({"jsonrpc":"2.0","id":"request-fixture","method":"instance.create","params":{}}).to_string().into())).await.expect("dispatch fixture");
                    while let Some(frame) = socket.recv().await {
                        match frame.expect("WS frame") {
                            Message::Text(text) => {
                                let frame: Value = serde_json::from_str(&text).expect("JSON frame");
                                if frame["id"] == "request-fixture" {
                                    assert_eq!(frame["error"]["code"], -32601);
                                    assert!(frame.get("result").is_none());
                                    if let Some(checked) = checked.lock().await.take() { let _ = checked.send(()); }
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                })
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let node = tokio::spawn(async move {
            outbound_session(
                &format!("ws://{address}/v1/node"),
                "synthetic-bootstrap".into(),
                json!({"hostId":"synthetic"}),
                None,
                Duration::from_secs(2),
                async {
                    let _ = stopped.await;
                    Ok(())
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), received)
            .await
            .expect("dispatch reply deadline")
            .expect("error reply checked");
        let _ = stop.send(());
        node.await
            .expect("Node task")
            .expect("Node connection shutdown");
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn enrollment_token_is_persisted_in_a_private_file() {
        let dir = std::env::temp_dir().join(format!(
            "remuda-node-identity-test-{}",
            remuda_protocol::HostId::new().as_id()
        ));
        std::fs::create_dir(&dir).expect("fixture directory");
        let token = dir.join("host-token");
        persist_host_token(&token, "synthetic-enrollment-token").expect("persist fixture token");
        assert_eq!(
            std::fs::read_to_string(&token).expect("token file"),
            "synthetic-enrollment-token\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&token)
                    .expect("token metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).expect("remove synthetic identity fixture");
    }

    #[test]
    fn cli_inventory_overrides_values_without_dropping_unrelated_labels() {
        let mut config = Config::default();
        config.node.labels.insert("region".into(), "file".into());
        config.node.labels.insert("gpu".into(), "none".into());
        let args = Args {
            no_herdr_orphan_sweep: false,
            stdio: true,
            display_label: None,
            hub_url: None,
            host_token_file: None,
            labels: vec!["region=cli".into()],
            max_instances: Some(3),
            herdr_socket: None,
        };
        args.apply(&mut config).expect("CLI overrides");
        assert_eq!(config.node.labels["region"], "cli");
        assert_eq!(config.node.labels["gpu"], "none");
        assert_eq!(config.node.max_instances, 3);
    }

    #[tokio::test]
    async fn shutdown_reaches_the_driver_and_waits_for_close_settlement() {
        let node = DevNode::new(&remuda_node::DevServerConfig::loopback(0)).expect("fake node");
        let request = serde_json::from_str(r#"{"prompt":"synthetic shutdown fixture"}"#)
            .expect("fixture request");
        let instance = node
            .create_instance(request)
            .await
            .expect("create")
            .instance;
        shutdown_drivers(&node, Duration::from_secs(3))
            .await
            .expect("driver shutdown");
        assert_eq!(
            node.get_instance(&instance.meta.id)
                .expect("instance")
                .lifecycle,
            remuda_protocol::InstanceLifecycle::Exited
        );
    }
}
