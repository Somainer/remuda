//! A NODE_BUSY refusal is pre-send: create must surface 503 (not a 200
//! reconciling command), and the fresh instance row settles as failed.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
}

fn cookie_from(head: &str) -> Option<String> {
    head.lines().find_map(|line| {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            line.split_once(':')?
                .1
                .trim()
                .split(';')
                .next()
                .map(str::to_string)
        } else {
            None
        }
    })
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "busy-phone" }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    cookie_from(&head).context("set-cookie")
}

async fn enroll_token(addr: std::net::SocketAddr, cookie: &str) -> Result<String> {
    let (status, _, rest) = http(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", cookie)],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "enroll-token {status} {rest}");
    Ok(serde_json::from_str::<Value>(rest.trim())?["token"]
        .as_str()
        .context("token")?
        .to_string())
}

type NodeWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn connect_node(addr: std::net::SocketAddr, bearer: &str, host_id: &str) -> Result<NodeWs> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse()?);
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": "busy-node",
                "host": { "maxInstances": 64, "cli": [] }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(node)
}

/// Reads every RPC frame off the socket and never replies, so the Hub's
/// pending table fills.
fn park_node(addr: std::net::SocketAddr, bearer: &str, host_id: &str) -> JoinHandle<()> {
    let bearer = bearer.to_string();
    let host_id = host_id.to_string();
    tokio::spawn(async move {
        let Ok(mut node) = connect_node(addr, &bearer, &host_id).await else {
            return;
        };
        // Answer host.resources immediately (placement/worktree calls issue one
        // before the RPC we want parked); every other frame stays unanswered so
        // its call occupies a Hub pending slot.
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").and_then(Value::as_str) == Some("host.resources") {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "resources": { "cpuPct": 1, "memPct": 1 } }
                });
                if node
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    })
}

#[tokio::test]
async fn saturated_control_budget_refuses_create_with_503_and_fails_row() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let _node = park_node(hub.addr, &enroll_token(hub.addr, &cookie).await?, &host_id);
    // Wait for the link to register as online.
    for _ in 0..100 {
        let (status, _, body) =
            http(hub.addr, "GET", "/v1/hosts", &[("Cookie", &cookie)], None).await?;
        assert_eq!(status, 200);
        let online = serde_json::from_str::<Value>(&body)?["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|h| h["hostId"] == host_id));
        if online {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Fill all 32 link slots with parked control RPCs (worktree.list).
    let parked: Vec<JoinHandle<()>> = (0..32)
        .map(|_| {
            let addr = hub.addr;
            let cookie = cookie.clone();
            let host_id = host_id.clone();
            tokio::spawn(async move {
                let _ = http(
                    addr,
                    "GET",
                    &format!("/v1/worktrees?hostId={host_id}"),
                    &[("Cookie", &cookie)],
                    None,
                )
                .await;
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The create frame is refused before it reaches the Node: 503 NODE_BUSY,
    // never a 200 with a reconciling command.
    let body = json!({
        "placement": { "kind": "any" },
        "driver": "claude-print",
        "delegation": "none"
    });
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&body.to_string()),
    )
    .await?;
    let error_body: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(status, 503, "create body: {error_body}");
    assert_eq!(error_body["code"], "NODE_BUSY", "{error_body}");
    assert_eq!(error_body["retryable"], true);
    assert!(error_body["retryAfterMs"].as_u64().is_some_and(|ms| ms > 0));

    // The just-inserted row is settled as failed, not left reconciling.
    let (status, _, list) = http(
        hub.addr,
        "GET",
        "/v1/instances",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let items = serde_json::from_str::<Value>(&list)?["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let fresh = items
        .iter()
        .find(|item| item["hostId"] == host_id)
        .context("refused create left no instance row")?;
    assert_eq!(fresh["lifecycle"], "failed", "{fresh}");

    for handle in parked {
        handle.abort();
    }
    Ok(())
}
