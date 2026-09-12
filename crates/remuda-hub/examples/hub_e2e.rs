//! In-process Hub plus a fake Node for Playwright live-hub e2e.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InteractionId};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BOOTSTRAP: &str = "e2e-bootstrap-token";
const LISTEN: &str = "127.0.0.1:18787";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("remuda_hub=info".parse()?),
        )
        .init();

    let dir = tempfile::tempdir().context("e2e data dir")?;
    let origins = std::env::var("HUB_E2E_ORIGINS").unwrap_or_else(|_| {
        "http://127.0.0.1:4179,http://localhost:4179,http://127.0.0.1:4177".into()
    });
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.listen = LISTEN.parse::<SocketAddr>().context("listen addr")?;
    config.bootstrap_token = BOOTSTRAP.into();
    config.cookie_secure = false;
    config.allowed_origins = origins
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let hub = spawn(config).await?;
    let host_id = HostId::new();
    let pending: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    let (ready_tx, ready_rx) = oneshot::channel();
    let node = tokio::spawn(fake_node(
        hub.addr,
        BOOTSTRAP.to_string(),
        host_id.clone(),
        pending,
        ready_tx,
    ));
    ready_rx.await.context("fake node hello")?;
    let line = json!({
        "hub": format!("http://{}", hub.addr),
        "token": BOOTSTRAP,
        "hostId": host_id.as_id().as_str(),
    });
    println!("HUB_E2E_READY {line}");
    let _ = io::stdout().flush();
    tokio::signal::ctrl_c().await.ok();
    node.abort();
    drop(hub);
    Ok(())
}

async fn fake_node(
    addr: SocketAddr,
    bootstrap: String,
    host_id: HostId,
    pending: Arc<Mutex<HashMap<String, Value>>>,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let mut req = format!("ws://{addr}/v1/node")
        .into_client_request()
        .context("node ws")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}")
            .parse()
            .context("authorization header")?,
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0-e2e",
                "label": "e2e-fake-node",
                "host": {
                    "hostname": "e2e-fake-node.local",
                    "labels": { "role": "e2e" },
                    "maxInstances": 4,
                    "cli": [{
                        "kind": "claude",
                        "version": "2.1.268",
                        "absolutePath": "/usr/bin/claude",
                        "authState": "logged_in"
                    }]
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _hello = ws.next().await;
    let _ = ready.send(());
    let mut append_n = 0u64;
    while let Some(msg) = ws.next().await {
        let Ok(Message::Text(text)) = msg else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if frame.get("method").is_none() {
            continue;
        }
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        let instance_id = params
            .get("instanceId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match method {
            "instance.create" => {
                let prompt = params
                    .pointer("/initialInput/text")
                    .or_else(|| params.get("prompt"))
                    .and_then(Value::as_str)
                    .unwrap_or("hello");
                let interaction_id = InteractionId::new();
                let card = fake_approval(
                    &instance_id,
                    host_id.as_id().as_str(),
                    interaction_id.as_id().as_str(),
                );
                pending
                    .lock()
                    .await
                    .insert(interaction_id.as_id().as_str().to_string(), card);
                append_n = append_journal(&mut ws, &instance_id, append_n, "user", prompt).await?;
                append_n = append_journal(
                    &mut ws,
                    &instance_id,
                    append_n,
                    "assistant",
                    &format!("echo: {prompt}"),
                )
                .await?;
                send_rpc_ok(
                    &mut ws,
                    id,
                    json!({ "ok": true, "instanceId": instance_id }),
                )
                .await?;
            }
            "instance.send" => {
                let prompt = params
                    .get("prompt")
                    .or_else(|| params.pointer("/text"))
                    .and_then(Value::as_str)
                    .unwrap_or("hello");
                append_n = append_journal(&mut ws, &instance_id, append_n, "user", prompt).await?;
                append_n = append_journal(
                    &mut ws,
                    &instance_id,
                    append_n,
                    "assistant",
                    &format!("echo: {prompt}"),
                )
                .await?;
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            "instance.close" | "instance.cancel" | "instance.resume" => {
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            "interaction.list" => {
                let items: Vec<Value> = pending.lock().await.values().cloned().collect();
                send_rpc_ok(&mut ws, id, json!({ "items": items })).await?;
            }
            "interaction.answer" => {
                if let Some(iid) = params.get("interactionId").and_then(Value::as_str) {
                    pending.lock().await.remove(iid);
                }
                send_rpc_ok(
                    &mut ws,
                    id,
                    json!({ "ok": true, "state": "answer-committed" }),
                )
                .await?;
            }
            _ => {
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
        }
    }
    Ok(())
}

async fn send_rpc_ok(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: Value,
    result: Value,
) -> Result<()> {
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
            .to_string()
            .into(),
    ))
    .await?;
    Ok(())
}

async fn append_journal(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    role: &str,
    text: &str,
) -> Result<u64> {
    let seq = n + 1;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": format!("j{seq}"),
            "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "event": {
                    "kind": "message",
                    "completeness": "structured",
                    "payload": { "role": role, "text": text }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    // Drain the Hub RPC result so it is not mistaken for a later request.
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    Ok(seq)
}

fn fake_approval(instance_id: &str, host_id: &str, interaction_id: &str) -> Value {
    json!({
        "id": interaction_id,
        "revision": "1",
        "createdAt": "2026-09-12T00:00:00.000Z",
        "updatedAt": "2026-09-12T00:00:00.000Z",
        "instanceId": instance_id,
        "runId": null,
        "hostId": host_id,
        "kind": "approval",
        "requestKey": {
            "native": { "type": "rpc", "valueType": "string", "value": "e2e-tool" },
            "processGeneration": "1",
            "runGeneration": "1",
            "connectionEpoch": host_id
        },
        "requestVersion": "1",
        "state": "pending",
        "blocking": true,
        "answerable": true,
        "carrier": "claude-control",
        "request": {
            "kind": "approval",
            "title": "Bash",
            "description": "echo e2e",
            "toolCallId": interaction_id,
            "actionRef": interaction_id,
            "options": [
                { "id": "allow-once", "label": "允许一次", "effect": "allow-once", "nativeValueRef": interaction_id },
                { "id": "deny", "label": "拒绝", "effect": "deny", "nativeValueRef": interaction_id }
            ],
            "requestedPermissionsRef": null,
            "inputDigest": "sha256:abababababababababababababababababababababababababababababababab"
        },
        "deadline": { "state": "unknown", "reason": "none", "evidenceEventIds": [] },
        "deadlineSource": "none",
        "answer": { "state": "not-applicable" },
        "delivery": "not-sent",
        "resolution": { "state": "not-applicable" }
    })
}
