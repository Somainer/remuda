//! NDJSON `node --stdio` carrier: emit `node.hello`, answer `hub.hello` / `hub.ping`.

use crate::inventory::{CollectRequest, collect};
use crate::{error::NodeError, load_or_create_host_id};
use remuda_protocol::Id;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MAX_STDIO_FRAME_BYTES: usize = 1_048_576;

/// Serve inventory on stdout and accept Hub application frames on stdin.
pub async fn run_stdio(
    labels: BTreeMap<String, String>,
    max_instances: usize,
) -> Result<(), NodeError> {
    let snapshot = collect(&CollectRequest {
        labels,
        max_instances,
        herdr_socket: None,
    });
    let data_dir = std::env::var_os("REMUDA_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("data"));
    let host_id = load_or_create_host_id(&data_dir.join("node"))?;
    let hello = json!({
        "jsonrpc": "2.0",
        "method": "node.hello",
        "params": {
            "nodeEpoch": Id::new("epoch")?,
            "nodeVersion": env!("CARGO_PKG_VERSION"),
            "protocol": {
                "major": 1,
                "minor": 0,
                "framing": "ndjson",
                "maxFrameBytes": MAX_STDIO_FRAME_BYTES,
            },
            "host": {
                "hostId": host_id,
                "hostname": snapshot.hostname,
                "labels": snapshot.labels,
                "maxInstances": snapshot.max_instances,
                "cli": snapshot.cli,
                "herdr": snapshot.herdr,
                "resources": snapshot.resources,
                "os": snapshot.os,
                "kernel": snapshot.kernel,
                "libc": snapshot.libc,
            }
        }
    });
    let mut output = tokio::io::stdout();
    write_ndjson(&mut output, &hello).await?;

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = input.next_line().await? {
        if line.len() > MAX_STDIO_FRAME_BYTES {
            write_ndjson(
                &mut output,
                &json!({
                    "type": "node.error",
                    "code": "frame_too_large",
                    "message": "NDJSON frame exceeds 1048576 bytes"
                }),
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
                    &json!({
                        "type": "node.error",
                        "code": "invalid_json",
                        "message": error.to_string()
                    }),
                )
                .await?;
                continue;
            }
        };
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| frame.get("method").and_then(Value::as_str));
        let response = match kind {
            Some("hub.hello") => json!({
                "type": "node.ready",
                "hostId": host_id,
                "carrier": "stdio-ndjson",
            }),
            Some("hub.ping") => json!({
                "type": "node.pong",
                "id": frame.get("id").cloned().unwrap_or(Value::Null),
            }),
            Some(other) => json!({
                "type": "node.error",
                "code": "unsupported_frame",
                "message": format!("stdio carrier does not handle {other}")
            }),
            None => json!({
                "type": "node.error",
                "code": "missing_frame_type",
                "message": "NDJSON application frame requires type or method"
            }),
        };
        write_ndjson(&mut output, &response).await?;
    }
    Ok(())
}

async fn write_ndjson(output: &mut tokio::io::Stdout, value: &Value) -> Result<(), NodeError> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}
