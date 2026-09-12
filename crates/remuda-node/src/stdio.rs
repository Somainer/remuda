//! NDJSON `node --stdio` carrier: JSON-RPC `node.auth` / `node.hello` / `instance.*`.

use crate::enroll::{self, Enrollment};
use crate::inventory::{CollectRequest, collect};
use crate::transport::hubnode::{self as hubnode_codec, decode_request};
use crate::{DevNode, DevServerConfig, NodeError};
use remuda_protocol::Id;
use remuda_protocol::hubnode::{HubNodeMethod, METHOD_NODE_HELLO};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MAX_STDIO_FRAME_BYTES: usize = 1_048_576;

/// Options for [`run_stdio_opts`].
#[derive(Debug, Clone)]
pub struct StdioOptions {
    /// Placement labels (`region=sg`).
    pub labels: BTreeMap<String, String>,
    /// Advertised instance ceiling.
    pub max_instances: usize,
    /// Registry display name.
    pub display_label: Option<String>,
    /// Carrier advertised to Hub (`ssh-stdio`).
    pub transport: String,
    /// Data directory for `enrollment.json`.
    pub data_dir: PathBuf,
}

impl Default for StdioOptions {
    fn default() -> Self {
        Self {
            labels: BTreeMap::new(),
            max_instances: 8,
            display_label: None,
            transport: "ssh-stdio".into(),
            data_dir: enroll::default_data_dir(),
        }
    }
}

/// Serve inventory on stdout and accept Hub JSON-RPC frames on stdin.
pub async fn run_stdio(
    labels: BTreeMap<String, String>,
    max_instances: usize,
) -> Result<(), NodeError> {
    run_stdio_opts(StdioOptions {
        labels,
        max_instances,
        ..StdioOptions::default()
    })
    .await
}

/// Stdio session with display label, transport, and persisted enrollment.
pub async fn run_stdio_opts(opts: StdioOptions) -> Result<(), NodeError> {
    let snapshot = collect(&CollectRequest {
        labels: opts.labels.clone(),
        max_instances: opts.max_instances,
        herdr_socket: None,
    });
    let enrollment = enroll::load_or_create(&opts.data_dir)?;
    let display_label = opts
        .display_label
        .clone()
        .unwrap_or_else(|| snapshot.hostname.clone());
    let node = DevNode::new(&DevServerConfig::loopback(0))?;
    let host = serde_json::to_value(&snapshot)?;
    let node_epoch = Id::new("epoch")?;
    let token = enrollment.node_token.clone().or_else(|| {
        std::env::var("REMUDA_BOOTSTRAP_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
    });

    let mut output = tokio::io::stdout();
    if let Some(token) = token.as_deref() {
        write_ndjson(&mut output, &hubnode_codec::encode_auth("auth-1", token)).await?;
    }
    let params = hubnode_codec::stdio_hello_params(
        &enrollment.host_id,
        &display_label,
        &opts.transport,
        env!("CARGO_PKG_VERSION"),
        &node_epoch,
        host,
        token.as_deref(),
    );
    write_ndjson(
        &mut output,
        &hubnode_codec::encode_hello_request("hello-1", params),
    )
    .await?;

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = input.next_line().await? {
        if line.len() > MAX_STDIO_FRAME_BYTES {
            write_ndjson(
                &mut output,
                &hubnode_codec::rpc_error(
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
                    &hubnode_codec::rpc_error(Value::Null, -32700, &error.to_string()),
                )
                .await?;
                continue;
            }
        };
        handle_stdio_frame(&node, &enrollment, &opts.data_dir, &mut output, frame).await?;
    }
    Ok(())
}

async fn handle_stdio_frame(
    node: &DevNode,
    enrollment: &Enrollment,
    data_dir: &std::path::Path,
    output: &mut tokio::io::Stdout,
    frame: Value,
) -> Result<(), NodeError> {
    if frame.get("method").is_none() {
        let _ = hubnode_codec::persist_hello_result(data_dir, &frame);
        return Ok(());
    }
    let request = match decode_request(&frame) {
        Ok(request) => request,
        Err(_) => {
            return reply_legacy(enrollment, output, &frame).await;
        }
    };
    let id = request.id.clone().unwrap_or(Value::Null);
    let params = request.params.clone().unwrap_or(json!({}));
    let kind = request.method_kind();
    if kind.is_some_and(HubNodeMethod::is_hello) || request.method == METHOD_NODE_HELLO {
        write_ndjson(
            output,
            &hubnode_codec::rpc_ok(
                id,
                json!({
                    "ok": true,
                    "hostId": enrollment.host_id,
                    "carrier": "ssh-stdio",
                }),
            ),
        )
        .await?;
        return Ok(());
    }
    match kind {
        Some(method)
            if HubNodeMethod::is_instance(method)
                || matches!(
                    method,
                    HubNodeMethod::JournalAppend | HubNodeMethod::TtyFrame
                ) =>
        {
            match hubnode_codec::dispatch_method(node, request.method.as_str(), params).await {
                Ok(result) => write_ndjson(output, &hubnode_codec::rpc_ok(id, result)).await?,
                Err(error) => {
                    write_ndjson(
                        output,
                        &hubnode_codec::rpc_error(id, rpc_code(&error), &error.to_string()),
                    )
                    .await?;
                }
            }
            Ok(())
        }
        _ if crate::interactions::is_interaction_method(request.method.as_str()) => {
            match node
                .dispatch_interaction(request.method.as_str(), params)
                .await
            {
                Ok(result) => write_ndjson(output, &hubnode_codec::rpc_ok(id, result)).await?,
                Err(error) => {
                    write_ndjson(
                        output,
                        &hubnode_codec::rpc_error(id, rpc_code(&error), &error.to_string()),
                    )
                    .await?;
                }
            }
            Ok(())
        }
        _ => reply_legacy(enrollment, output, &frame).await,
    }
}

async fn reply_legacy(
    enrollment: &Enrollment,
    output: &mut tokio::io::Stdout,
    frame: &Value,
) -> Result<(), NodeError> {
    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| frame.get("method").and_then(Value::as_str));
    let response = match kind {
        Some("hub.hello") => json!({
            "type": "node.ready",
            "hostId": enrollment.host_id,
            "carrier": "stdio-ndjson",
        }),
        Some("hub.ping") => json!({
            "type": "node.pong",
            "id": frame.get("id").cloned().unwrap_or(Value::Null),
        }),
        Some(other) => hubnode_codec::rpc_error(
            frame.get("id").cloned().unwrap_or(Value::Null),
            -32601,
            &format!("stdio carrier does not handle {other}"),
        ),
        None => hubnode_codec::rpc_error(
            frame.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "NDJSON application frame requires type or method",
        ),
    };
    write_ndjson(output, &response).await
}

fn rpc_code(error: &NodeError) -> i64 {
    match error {
        NodeError::InvalidRequest(_) | NodeError::NotFound { .. } => -32602,
        NodeError::QueueFull => -32001,
        NodeError::InteractionExpired => -32005,
        NodeError::InteractionSuperseded { .. } => -32004,
        _ => -32603,
    }
}

async fn write_ndjson(output: &mut tokio::io::Stdout, value: &Value) -> Result<(), NodeError> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}
