//! NDJSON `node --stdio` carrier: JSON-RPC `node.auth` / `node.hello` / `instance.*`.

use crate::enroll::{self, Enrollment};
use crate::inventory::{CollectRequest, collect};
use crate::transport::hubnode::{self as hubnode_codec, decode_request};
use crate::{DevNode, DevServerConfig, NodeError};
use remuda_protocol::hubnode::{HubNodeMethod, METHOD_JOURNAL_APPEND, METHOD_NODE_HELLO};
use remuda_protocol::{Id, InstanceId, JournalEvent};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc};

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
    run_stdio_io(
        opts,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

/// Same session over caller-supplied NDJSON streams (tests).
pub async fn run_stdio_io<R, W>(
    opts: StdioOptions,
    input: R,
    mut output: W,
) -> Result<(), NodeError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send,
{
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
    let node = DevNode::with_host_id(&DevServerConfig::loopback(0), enrollment.host_id.clone())?;
    let host = serde_json::to_value(&snapshot)?;
    let node_epoch = Id::new("epoch")?;
    let token = enrollment.node_token.clone().or_else(|| {
        std::env::var("REMUDA_BOOTSTRAP_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
    });

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

    let (journal_tx, mut journal_rx) = mpsc::channel::<Value>(32);
    let pumps = Arc::new(Mutex::new(HashSet::new()));
    let ids = Arc::new(AtomicU64::new(1));
    let mut lines = input.lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
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
                let created = handle_stdio_frame(
                    &node,
                    &enrollment,
                    &opts.data_dir,
                    &mut output,
                    frame,
                )
                .await?;
                if let Some(instance_id) = created {
                    start_journal_pump(
                        node.clone(),
                        instance_id,
                        journal_tx.clone(),
                        Arc::clone(&pumps),
                        Arc::clone(&ids),
                    )
                    .await;
                }
            }
            frame = journal_rx.recv() => {
                let Some(frame) = frame else { break; };
                write_ndjson(&mut output, &frame).await?;
            }
        }
    }
    Ok(())
}

async fn handle_stdio_frame<W: AsyncWrite + Unpin>(
    node: &DevNode,
    enrollment: &Enrollment,
    data_dir: &std::path::Path,
    output: &mut W,
    frame: Value,
) -> Result<Option<InstanceId>, NodeError> {
    if frame.get("method").is_none() {
        let _ = hubnode_codec::persist_hello_result(data_dir, &frame);
        return Ok(None);
    }
    let request = match decode_request(&frame) {
        Ok(request) => request,
        Err(_) => {
            reply_legacy(enrollment, output, &frame).await?;
            return Ok(None);
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
        return Ok(None);
    }
    if kind.is_some_and(|method| {
        method.is_instance()
            || matches!(
                method,
                HubNodeMethod::JournalAppend | HubNodeMethod::TtyFrame
            )
    }) {
        match hubnode_codec::dispatch_method(node, request.method.as_str(), params).await {
            Ok(result) => {
                let created = created_instance_id(&result);
                write_ndjson(output, &hubnode_codec::rpc_ok(id, result)).await?;
                return Ok(created);
            }
            Err(error) => {
                write_ndjson(
                    output,
                    &hubnode_codec::rpc_error(id, rpc_code(&error), &error.to_string()),
                )
                .await?;
                return Ok(None);
            }
        }
    }
    if crate::interactions::is_interaction_method(request.method.as_str()) {
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
        return Ok(None);
    }
    reply_legacy(enrollment, output, &frame).await?;
    Ok(None)
}

fn created_instance_id(result: &Value) -> Option<InstanceId> {
    result
        .pointer("/instance/id")
        .or_else(|| result.pointer("/instance/instanceId"))
        .or_else(|| result.pointer("/instance/meta/id"))
        .and_then(Value::as_str)
        .and_then(|raw| InstanceId::from_str(raw).ok())
}

async fn start_journal_pump(
    node: DevNode,
    instance_id: InstanceId,
    journal_tx: mpsc::Sender<Value>,
    pumps: Arc<Mutex<HashSet<String>>>,
    ids: Arc<AtomicU64>,
) {
    let key = instance_id.as_id().as_str().to_owned();
    {
        let mut guard = pumps.lock().await;
        if !guard.insert(key.clone()) {
            return;
        }
    }
    let Ok(rx) = node.subscribe(&instance_id) else {
        pumps.lock().await.remove(&key);
        return;
    };
    tokio::spawn(async move {
        pump_journal(node, instance_id, rx, journal_tx, ids).await;
    });
}

async fn pump_journal(
    node: DevNode,
    instance_id: InstanceId,
    mut rx: broadcast::Receiver<JournalEvent>,
    journal_tx: mpsc::Sender<Value>,
    ids: Arc<AtomicU64>,
) {
    if let Ok(instance) = node.get_instance(&instance_id)
        && let Ok(page) = node.read_journal(&instance.journal_id, None, 256)
    {
        for event in page.events {
            if forward_journal(&instance_id, &event, &journal_tx, &ids)
                .await
                .is_err()
            {
                return;
            }
        }
    }
    loop {
        match rx.recv().await {
            Ok(event) => {
                if forward_journal(&instance_id, &event, &journal_tx, &ids)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn forward_journal(
    instance_id: &InstanceId,
    event: &JournalEvent,
    journal_tx: &mpsc::Sender<Value>,
    ids: &AtomicU64,
) -> Result<(), NodeError> {
    let seq = i64::try_from(event.position().1.0).unwrap_or(0);
    let value = serde_json::to_value(event)?;
    let id = format!("j-{}", ids.fetch_add(1, Ordering::Relaxed));
    let frame = hubnode_codec::rpc_request(
        id,
        METHOD_JOURNAL_APPEND,
        hubnode_codec::encode_append(instance_id.as_id().as_str(), Some(seq), value),
    );
    journal_tx
        .send(frame)
        .await
        .map_err(|_| NodeError::Disconnected)
}

async fn reply_legacy<W: AsyncWrite + Unpin>(
    enrollment: &Enrollment,
    output: &mut W,
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

async fn write_ndjson<W: AsyncWrite + Unpin>(
    output: &mut W,
    value: &Value,
) -> Result<(), NodeError> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::HostId;
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    async fn read_json<R: AsyncBufRead + Unpin>(reader: &mut R) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("line");
        serde_json::from_str(line.trim()).expect("json")
    }

    #[tokio::test]
    async fn stdio_create_uses_enrollment_host_and_appends_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let enrollment = enroll::load_or_create(dir.path()).expect("enroll");
        let opts = StdioOptions {
            display_label: Some("stdio-test".into()),
            data_dir: dir.path().to_path_buf(),
            transport: "ssh-stdio".into(),
            ..StdioOptions::default()
        };
        let (client_in, node_out) = duplex(64 * 1024);
        let (node_in, mut client_out) = duplex(64 * 1024);
        let session =
            tokio::spawn(
                async move { run_stdio_io(opts, BufReader::new(node_in), node_out).await },
            );

        let mut from_node = BufReader::new(client_in);
        let hello = read_json(&mut from_node).await;
        assert_eq!(hello["method"], "node.hello");
        assert_eq!(
            hello["params"]["hostId"],
            json!(enrollment.host_id.as_id().as_str())
        );

        let hub_hello = json!({
            "jsonrpc": "2.0",
            "id": "hello-1",
            "result": {
                "hostId": enrollment.host_id,
                "nodeToken": "stdio-secret"
            }
        });
        client_out
            .write_all(format!("{hub_hello}\n").as_bytes())
            .await
            .expect("write hello result");

        let create = json!({
            "jsonrpc": "2.0",
            "id": "c-1",
            "method": "instance.create",
            "params": {
                "instanceId": remuda_protocol::InstanceId::new().as_id().as_str(),
                "spec": {
                    "kind": "claude",
                    "driver": "claude-print",
                    "prompt": "hi from stdio",
                    "hostId": HostId::new().as_id().as_str()
                },
                "initialInput": { "type": "prompt", "text": "hi from stdio" }
            }
        });
        client_out
            .write_all(format!("{create}\n").as_bytes())
            .await
            .expect("write create");

        let mut saw_create = false;
        let mut saw_journal = false;
        for _ in 0..16 {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(2), read_json(&mut from_node))
                    .await
                    .expect("frame");
            if frame["id"] == "c-1" && frame.get("result").is_some() {
                saw_create = true;
                let host = frame
                    .pointer("/result/instance/hostId")
                    .or_else(|| frame.pointer("/result/instance/host_id"));
                if let Some(host) = host {
                    assert_eq!(host, &json!(enrollment.host_id.as_id().as_str()));
                }
            }
            if frame["method"] == "journal.append" {
                saw_journal = true;
            }
            if saw_create && saw_journal {
                break;
            }
        }
        assert!(saw_create, "instance.create result");
        assert!(saw_journal, "journal.append from stdio pump");

        let loaded = enroll::load_or_create(dir.path()).expect("reload");
        assert_eq!(loaded.node_token.as_deref(), Some("stdio-secret"));
        session.abort();
    }
}
