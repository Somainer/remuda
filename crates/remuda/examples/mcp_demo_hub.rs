//! In-process Hub plus a fake online Node for `scripts/demo/mcp-workflow.sh`.
//!
//! Prints one `READY {json}` line on stdout, then waits for Ctrl-C. Logs go to
//! stderr. The fake Node answers Hub `instance.*` RPCs and appends a terminal
//! journal event so `remuda_instance_wait` can observe a run.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const RPC: Duration = Duration::from_secs(8);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init()
        .ok();

    let data_dir = std::env::var("REMUDA_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("remuda-mcp-demo-hub"));
    std::fs::create_dir_all(&data_dir).context("data dir")?;
    let mut config = HubConfig::for_test(data_dir);
    if let Ok(token) = std::env::var("REMUDA_BOOTSTRAP_TOKEN")
        && !token.is_empty()
    {
        config.bootstrap_token = token;
    }
    if let Ok(listen) = std::env::var("REMUDA_LISTEN")
        && !listen.is_empty()
    {
        config.listen = listen.parse().context("REMUDA_LISTEN")?;
    }
    let hub = spawn(config).await?;
    let bootstrap = hub.bootstrap_token.clone();
    let host_id = HostId::new();
    let node = enroll_fake_node(hub.addr, &bootstrap, host_id.as_id().as_str()).await?;

    let ready = json!({
        "hub": format!("http://{}", hub.addr),
        "bootstrapToken": bootstrap,
        "hostId": host_id.as_id().as_str(),
        "labels": ["region=sg"],
    });
    println!("READY {ready}");
    std::io::stdout().flush()?;

    tokio::signal::ctrl_c().await.ok();
    node.abort();
    drop(hub);
    Ok(())
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = timeout(RPC, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match msg {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

async fn enroll_fake_node(
    addr: std::net::SocketAddr,
    bootstrap: &str,
    host_id: &str,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bootstrap}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-demo",
                "label": "fake-node",
                "host": {
                    "hostname": "fake-node.local",
                    "labels": { "region": "sg" },
                    "maxInstances": 4,
                    "cli": [{
                        "kind": "claude",
                        "version": "0.0.0",
                        "absolutePath": "/usr/bin/claude",
                        "authState": "unknown"
                    }]
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(
        hello["result"]["nodeToken"].as_str().is_some(),
        "hello {hello}"
    );

    Ok(tokio::spawn(async move {
        let mut seq: u64 = 0;
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if let Some(id) = frame.get("id").cloned()
                && let Some(method) = frame.get("method").and_then(Value::as_str)
            {
                let params = frame.get("params").cloned().unwrap_or(json!({}));
                let _ = node
                    .send(Message::Text(
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                            .to_string()
                            .into(),
                    ))
                    .await;
                if method == "instance.create" {
                    let instance_id = params
                        .get("instanceId")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            params
                                .get("payload")
                                .and_then(|p| p.get("instanceId"))
                                .and_then(Value::as_str)
                        })
                        .unwrap_or("");
                    if !instance_id.is_empty() {
                        seq += 1;
                        let append_id = format!("j{seq}");
                        let _ = node
                            .send(Message::Text(
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": append_id,
                                    "method": "journal.append",
                                    "params": {
                                        "instanceId": instance_id,
                                        "event": {
                                            "type": "run.terminal",
                                            "payload": { "text": "OK" }
                                        }
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await;
                        let _ = recv_json(&mut node).await;
                    }
                }
            }
        }
    }))
}
