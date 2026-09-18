//! Node-to-Hub carrier abstraction and the SSH-friendly stdio transport.

use crate::NodeError;
use remuda_protocol::{HostId, Id};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const MAX_STDIO_FRAME_BYTES: usize = 1024 * 1024;

/// Future returned by a transport implementation.
pub type CarrierFuture<'a> = Pin<Box<dyn Future<Output = Result<(), NodeError>> + Send + 'a>>;

/// Replaceable transport for the same Node-to-Hub application frames.
pub trait HubCarrier: Send {
    /// Stable transport label reported for diagnostics.
    fn kind(&self) -> CarrierKind;
    /// Connect and carry frames until EOF, shutdown, or a transport error.
    fn run<'a>(&'a mut self, hello: &'a NodeHello) -> CarrierFuture<'a>;
}

/// Supported carrier implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CarrierKind {
    /// Production/M1 Node-initiated WebSocket.
    OutboundWss,
    /// NDJSON on stdin/stdout, normally under `ssh host remuda node --stdio`.
    StdioNdjson,
}

/// M1 outbound WSS carrier boundary.
#[derive(Debug, Clone)]
pub struct OutboundWssCarrier {
    endpoint: String,
}

impl OutboundWssCarrier {
    /// Configure the Hub WSS URL. Connection/auth lands with M1 enrollment.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

impl HubCarrier for OutboundWssCarrier {
    fn kind(&self) -> CarrierKind {
        CarrierKind::OutboundWss
    }

    fn run<'a>(&'a mut self, hello: &'a NodeHello) -> CarrierFuture<'a> {
        Box::pin(async move {
            let data_dir = crate::enroll::default_data_dir();
            let enrollment = crate::enroll::load_or_create(&data_dir)?;
            let token = enrollment
                .node_token
                .clone()
                .or_else(crate::enroll::enroll_token_from_env)
                .ok_or_else(|| {
                    NodeError::InvalidConfig(format!(
                        "outbound WSS {} requires REMUDA_ENROLL_TOKEN or enrollment.json",
                        self.endpoint
                    ))
                })?;
            let url = if self.endpoint.contains("/v1/node") || self.endpoint.starts_with("ws") {
                self.endpoint.clone()
            } else {
                let trimmed = self.endpoint.trim_end_matches('/');
                if let Some(rest) = trimmed.strip_prefix("https://") {
                    format!("wss://{rest}/v1/node")
                } else if let Some(rest) = trimmed.strip_prefix("http://") {
                    format!("ws://{rest}/v1/node")
                } else {
                    format!("ws://{trimmed}/v1/node")
                }
            };
            let config = crate::WssConfig {
                url,
                token,
                host_id: hello.params.host.host_id.as_id().as_str().to_owned(),
                label: hello.params.host.hostname.clone(),
                node_version: env!("CARGO_PKG_VERSION").into(),
                cli: serde_json::to_value(&hello.params.host.cli).unwrap_or_else(|_| json!([])),
                host: serde_json::to_value(&hello.params.host).ok(),
                heartbeat_interval: std::time::Duration::from_secs(15),
                backoff: crate::Backoff::default(),
                journal_queue: 32,
            };
            let mut link = crate::WssLink::connect(config).await?;
            if let Some(token) = &link.node_token {
                let _ = crate::enroll::apply_hello_result(
                    &data_dir,
                    &json!({ "hostId": link.host_id, "nodeToken": token }),
                );
            }
            let node = crate::DevNode::new(&crate::DevServerConfig::loopback(0))?;
            while let Some(request) = link.next_hub_request().await {
                let result = crate::transport::hubnode::dispatch_method(
                    &node,
                    &request.method,
                    request.params.clone(),
                )
                .await;
                let _ = request.respond(result).await;
            }
            Ok(())
        })
    }
}

/// SSH-friendly carrier. Each UTF-8 line is exactly one JSON application frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdioCarrier;

impl StdioCarrier {
    /// Construct an NDJSON stdio carrier.
    pub fn new() -> Self {
        Self
    }
}

impl HubCarrier for StdioCarrier {
    fn kind(&self) -> CarrierKind {
        CarrierKind::StdioNdjson
    }

    fn run<'a>(&'a mut self, hello: &'a NodeHello) -> CarrierFuture<'a> {
        Box::pin(async move {
            let data_dir = crate::enroll::default_data_dir();
            let enrollment = crate::enroll::load_or_create(&data_dir)?;
            let mut input = BufReader::new(tokio::io::stdin()).lines();
            let mut output = tokio::io::stdout();
            let token = enrollment
                .node_token
                .clone()
                .or_else(crate::enroll::enroll_token_from_env);
            if let Some(token) = token.as_deref() {
                write_ndjson(
                    &mut output,
                    &crate::transport::hubnode::encode_auth("auth-1", token),
                )
                .await?;
            }
            let host = serde_json::to_value(&hello.params.host)?;
            let params = crate::transport::hubnode::stdio_hello_params(
                &enrollment.host_id,
                &hello.params.host.hostname,
                "ssh-stdio",
                env!("CARGO_PKG_VERSION"),
                &hello.params.node_epoch,
                host,
                token.as_deref(),
                // This carrier composes its own `DevNode` *after* the hello is
                // written, so it holds no rows to enumerate. `None` (no key)
                // is the honest answer; `[]` would claim it owns nothing.
                None,
            );
            write_ndjson(
                &mut output,
                &crate::transport::hubnode::encode_hello_request("hello-1", params),
            )
            .await?;

            let node = crate::DevNode::new(&crate::DevServerConfig::loopback(0))?;
            while let Some(line) = input.next_line().await? {
                if line.len() > MAX_STDIO_FRAME_BYTES {
                    write_ndjson(
                        &mut output,
                        &crate::transport::hubnode::rpc_error(
                            Value::Null,
                            -32600,
                            "NDJSON frame exceeds 1048576 bytes",
                        ),
                    )
                    .await?;
                    return Err(NodeError::InvalidRequest(
                        "stdio NDJSON frame exceeds local limit".to_owned(),
                    ));
                }
                if line.trim().is_empty() {
                    continue;
                }
                let frame: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(error) => {
                        write_ndjson(
                            &mut output,
                            &crate::transport::hubnode::rpc_error(
                                Value::Null,
                                -32700,
                                &error.to_string(),
                            ),
                        )
                        .await?;
                        continue;
                    }
                };
                if frame.get("method").is_none() {
                    let _ = crate::transport::hubnode::persist_hello_result(&data_dir, &frame);
                    continue;
                }
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                let params = frame.get("params").cloned().unwrap_or(json!({}));
                match crate::transport::hubnode::dispatch_method(&node, method, params).await {
                    Ok(result) => {
                        write_ndjson(&mut output, &crate::transport::hubnode::rpc_ok(id, result))
                            .await?;
                    }
                    Err(error) => {
                        let code = match error {
                            NodeError::InvalidRequest(_) | NodeError::NotFound { .. } => -32602,
                            _ => -32601,
                        };
                        if matches!(method, "hub.hello" | "hub.ping") || frame.get("type").is_some()
                        {
                            let response = match frame.get("type").and_then(Value::as_str) {
                                Some("hub.hello") => json!({
                                    "type": "node.ready",
                                    "nodeEpoch": hello.params.node_epoch,
                                    "carrier": CarrierKind::StdioNdjson,
                                }),
                                Some("hub.ping") => json!({
                                    "type": "node.pong",
                                    "id": id,
                                }),
                                _ => crate::transport::hubnode::rpc_error(
                                    id,
                                    code,
                                    &error.to_string(),
                                ),
                            };
                            write_ndjson(&mut output, &response).await?;
                        } else {
                            write_ndjson(
                                &mut output,
                                &crate::transport::hubnode::rpc_error(id, code, &error.to_string()),
                            )
                            .await?;
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

async fn write_ndjson<W, T>(output: &mut W, value: &T) -> Result<(), NodeError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

/// Configuration-backed inventory fields included in the initial `node.hello` frame.
#[derive(Debug, Clone)]
pub struct HostInventoryConfig {
    /// Operator labels used later by placement.
    pub labels: BTreeMap<String, String>,
    /// Maximum number of concurrently managed Instances.
    pub max_instances: usize,
    /// Explicit Herdr server socket, when configured.
    pub herdr_socket: Option<PathBuf>,
}

impl Default for HostInventoryConfig {
    fn default() -> Self {
        Self {
            labels: BTreeMap::new(),
            max_instances: 8,
            herdr_socket: None,
        }
    }
}

/// Initial Node inventory report used by WSS and stdio carriers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeHello {
    /// JSON-RPC version; `node.hello` is an unsolicited notification.
    pub jsonrpc: String,
    /// Stable notification method.
    pub method: String,
    /// Inventory-bearing hello payload.
    pub params: NodeHelloParams,
}

/// Payload of the initial `node.hello` notification.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeHelloParams {
    /// Ephemeral process epoch; durable Node identity is an M1 enrollment concern.
    pub node_epoch: Id,
    /// Application protocol and framing declaration.
    pub protocol: NodeHelloProtocol,
    /// Host scheduling and executable inventory.
    pub host: HostInventory,
}

impl NodeHello {
    /// Probe local executable versions without invoking a model or reading credential content.
    pub fn detect(config: HostInventoryConfig) -> Result<Self, NodeError> {
        Self::detect_for_carrier(config, CarrierKind::StdioNdjson)
    }

    /// Probe inventory and describe the framing used by the selected carrier.
    pub fn detect_for_carrier(
        config: HostInventoryConfig,
        carrier: CarrierKind,
    ) -> Result<Self, NodeError> {
        Self::build(config, carrier, true)
    }

    fn build(
        config: HostInventoryConfig,
        carrier: CarrierKind,
        probe: bool,
    ) -> Result<Self, NodeError> {
        validate_inventory_config(&config)?;
        let snapshot = probe.then(|| {
            crate::inventory::collect(&crate::inventory::CollectRequest {
                labels: config.labels.clone(),
                max_instances: config.max_instances,
                herdr_socket: config.herdr_socket.clone(),
            })
        });
        let cli = match &snapshot {
            Some(snapshot) => snapshot.cli.iter().map(cli_from_snapshot).collect(),
            None => ["claude", "codex", "grok", "agy"]
                .into_iter()
                .map(|kind| detect_cli(kind, false))
                .collect(),
        };
        let herdr = match &snapshot {
            Some(snapshot) => HerdrInventory {
                absolute_path: snapshot.herdr.path.clone(),
                version: snapshot.herdr.version.clone(),
                socket: snapshot.herdr.socket.clone(),
            },
            None => detect_herdr(config.herdr_socket.clone(), false),
        };
        Ok(Self {
            jsonrpc: "2.0".to_owned(),
            method: "node.hello".to_owned(),
            params: NodeHelloParams {
                node_epoch: Id::new("epoch")?,
                protocol: NodeHelloProtocol {
                    major: 1,
                    minor: 0,
                    framing: match carrier {
                        CarrierKind::OutboundWss => "websocket-message",
                        CarrierKind::StdioNdjson => "ndjson",
                    }
                    .to_owned(),
                    max_frame_bytes: MAX_STDIO_FRAME_BYTES,
                },
                host: HostInventory {
                    host_id: HostId::new(),
                    hostname: snapshot
                        .as_ref()
                        .map(|s| s.hostname.clone())
                        .unwrap_or_else(|| {
                            std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_owned())
                        }),
                    labels: config.labels,
                    max_instances: config.max_instances,
                    cli,
                    herdr,
                    resources: snapshot.as_ref().map(|s| s.resources.clone()),
                    os: snapshot.as_ref().map(|s| s.os.clone()),
                    kernel: snapshot.as_ref().and_then(|s| s.kernel.clone()),
                    libc: snapshot.as_ref().and_then(|s| s.libc.clone()),
                    driver_inventory: crate::inventory::driver_inventory(),
                },
            },
        })
    }
}

fn validate_inventory_config(config: &HostInventoryConfig) -> Result<(), NodeError> {
    if config.max_instances == 0 {
        return Err(NodeError::InvalidConfig(
            "maxInstances must be positive".to_owned(),
        ));
    }
    if config.labels.len() > 64
        || config.labels.iter().any(|(key, value)| {
            key.is_empty() || key.len() > 64 || value.is_empty() || value.len() > 256
        })
    {
        return Err(NodeError::InvalidConfig(
            "host labels exceed the local inventory limits".to_owned(),
        ));
    }
    if config
        .herdr_socket
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(NodeError::InvalidConfig(
            "Herdr socket must be an absolute path".to_owned(),
        ));
    }
    Ok(())
}

/// `node.hello` protocol metadata.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeHelloProtocol {
    /// Remuda wire major.
    pub major: u16,
    /// Remuda wire minor.
    pub minor: u16,
    /// Stdio framing is newline-delimited JSON, never mixed with logs.
    pub framing: String,
    /// Maximum UTF-8 line size accepted from stdin.
    pub max_frame_bytes: usize,
}

/// Scheduling inventory for one host.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInventory {
    /// Host identity for this un-enrolled process.
    pub host_id: HostId,
    /// Best-effort hostname, or `unknown`.
    pub hostname: String,
    /// Exact operator-provided placement labels.
    pub labels: BTreeMap<String, String>,
    /// Configured concurrency ceiling.
    pub max_instances: usize,
    /// Known native CLI executables.
    pub cli: Vec<CliInventory>,
    /// Herdr binary and configured socket.
    pub herdr: HerdrInventory,
    /// CPU / memory snapshot from [`crate::inventory::collect`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::inventory::ResourceReport>,
    /// `std::env::consts::OS` when inventory was probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// `uname -r` when inventory was probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// libc identifier when inventory was probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
    /// D-028 §5.1 driver launch inventory, echoed under hello
    /// `capabilities.driverInventory`. Skipped when empty so silence reads as
    /// "not reported", never as a refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub driver_inventory: Vec<remuda_protocol::DriverDescriptor>,
}

/// One native CLI advertised by a Node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliInventory {
    /// Native product name.
    pub kind: String,
    /// Canonical absolute executable path when found on PATH.
    pub absolute_path: Option<PathBuf>,
    /// First non-empty `--version` output line.
    pub version: Option<String>,
    /// Auth is deliberately fail-closed until a driver-specific probe exists.
    pub auth_state: AuthState,
    /// SHA-256 of the executable (`sha256:` + hex), when probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Credential-independent CLI authentication state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthState {
    /// Executable is absent.
    NotInstalled,
    /// Executable exists, but M0 has no non-secret native auth probe.
    Unknown,
    /// Non-secret login marker is present (`inventory::CliAuth::LoggedIn`).
    LoggedIn,
    /// Binary present and the login marker is absent.
    LoggedOut,
}

/// Herdr inventory advertised independently of agent CLIs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HerdrInventory {
    /// Canonical absolute Herdr executable path.
    pub absolute_path: Option<PathBuf>,
    /// First non-empty `herdr --version` output line.
    pub version: Option<String>,
    /// Explicit configured socket; never inferred from another user's session.
    pub socket: Option<PathBuf>,
}

fn cli_from_snapshot(entry: &crate::inventory::CliEntry) -> CliInventory {
    CliInventory {
        kind: entry.kind.clone(),
        absolute_path: entry.path.clone(),
        version: entry.version.clone(),
        auth_state: if entry.path.is_none() {
            AuthState::NotInstalled
        } else {
            match entry.auth {
                crate::inventory::CliAuth::GatewayNative | crate::inventory::CliAuth::LoggedIn => {
                    AuthState::LoggedIn
                }
                crate::inventory::CliAuth::LoggedOut => AuthState::LoggedOut,
                crate::inventory::CliAuth::Unknown => AuthState::Unknown,
            }
        },
        sha256: entry.sha256.clone(),
    }
}

fn detect_cli(kind: &str, probe: bool) -> CliInventory {
    let absolute_path = probe.then(|| find_executable(kind)).flatten();
    let version = absolute_path.as_deref().and_then(binary_version);
    let auth_state = if absolute_path.is_some() {
        AuthState::Unknown
    } else {
        AuthState::NotInstalled
    };
    CliInventory {
        kind: kind.to_owned(),
        absolute_path,
        version,
        auth_state,
        sha256: None,
    }
}

fn detect_herdr(socket: Option<PathBuf>, probe: bool) -> HerdrInventory {
    let absolute_path = probe.then(|| find_executable("herdr")).flatten();
    let version = absolute_path.as_deref().and_then(binary_version);
    HerdrInventory {
        absolute_path,
        version,
        socket,
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
        .and_then(|candidate| std::fs::canonicalize(candidate).ok())
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

fn binary_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8(output.stderr).ok()?
    } else {
        String::from_utf8(output.stdout).ok()?
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(256).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_hello_has_complete_fail_closed_inventory() {
        let mut labels = BTreeMap::new();
        labels.insert("region".to_owned(), "test".to_owned());
        let hello = NodeHello::build(
            HostInventoryConfig {
                labels,
                max_instances: 3,
                herdr_socket: Some(PathBuf::from("/tmp/herdr.sock")),
            },
            CarrierKind::StdioNdjson,
            false,
        )
        .expect("hello");
        let value = serde_json::to_value(hello).expect("serialize hello");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "node.hello");
        assert_eq!(value["params"]["protocol"]["framing"], "ndjson");
        assert_eq!(value["params"]["host"]["maxInstances"], 3);
        assert_eq!(
            value["params"]["host"]["cli"].as_array().map(Vec::len),
            Some(4)
        );
        assert_eq!(
            value["params"]["host"]["cli"][0]["authState"],
            "not-installed"
        );
        assert_eq!(
            value["params"]["host"]["herdr"]["socket"],
            "/tmp/herdr.sock"
        );
    }

    #[test]
    fn outbound_wss_and_stdio_share_hello_with_distinct_framing() {
        let wss = NodeHello::build(
            HostInventoryConfig::default(),
            CarrierKind::OutboundWss,
            false,
        )
        .expect("WSS hello");
        let stdio = NodeHello::build(
            HostInventoryConfig::default(),
            CarrierKind::StdioNdjson,
            false,
        )
        .expect("stdio hello");
        assert_eq!(wss.params.protocol.framing, "websocket-message");
        assert_eq!(stdio.params.protocol.framing, "ndjson");
        assert_eq!(
            OutboundWssCarrier::new("wss://hub.invalid").kind(),
            CarrierKind::OutboundWss
        );
        assert_eq!(StdioCarrier::new().kind(), CarrierKind::StdioNdjson);
    }
}
