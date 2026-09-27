//! A NODE_BUSY refusal is pre-send: create must surface 503 (not a 200
//! reconciling command), and the fresh instance row settles as failed.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

async fn connect_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
) -> Result<(NodeWs, String)> {
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
            "method": "runtime.hello",
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
    // Read the hello result so a reconnect can authenticate with the minted
    // node token instead of the one-shot enrollment token.
    let hello = loop {
        match node.next().await {
            Some(Ok(Message::Text(text))) => {
                let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if frame.get("id") == Some(&json!("1")) {
                    break frame;
                }
            }
            other => anyhow::bail!("hello closed unexpectedly: {other:?}"),
        }
    };
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .unwrap_or(bearer)
        .to_string();
    Ok((node, node_token))
}

/// Reads every RPC frame off the socket and never replies, so the Hub's
/// pending table fills. Returns a counter of inbound RPC frames the Node saw.
fn park_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
) -> (JoinHandle<()>, Arc<AtomicUsize>) {
    let bearer = bearer.to_string();
    let host_id = host_id.to_string();
    let frames = Arc::new(AtomicUsize::new(0));
    let node_frames = frames.clone();
    let handle = tokio::spawn(async move {
        let Ok((mut node, _token)) = connect_node(addr, &bearer, &host_id).await else {
            return;
        };
        // Answer host.resources immediately (placement/worktree calls issue one
        // before the RPC we want parked); every other frame stays unanswered so
        // its call occupies a Hub pending slot.
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_some() && frame.get("id").is_some() {
                node_frames.fetch_add(1, Ordering::SeqCst);
            }
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
    });
    (handle, frames)
}

#[tokio::test]
async fn saturated_control_budget_refuses_create_with_503_and_fails_row() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let (_node, node_frames) =
        park_node(hub.addr, &enroll_token(hub.addr, &cookie).await?, &host_id);
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

    // Prove the precondition instead of sleeping: wait until the fake Node has
    // actually received all 32 parked worktree.list frames, so each occupies a
    // Hub pending slot. (A gated HTTP probe would itself park for 60 s.)
    tokio::time::timeout(Duration::from_secs(10), async {
        while node_frames.load(Ordering::SeqCst) < 32 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("fake Node never received the 32 parked control frames");

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

/// Drive a connected Node that answers `host.resources` and accepts every
/// command RPC, long enough to create an instance against before the link is
/// swapped for a parked one. Returns the driver task and the Node's minted
/// reconnect token.
async fn accepting_create_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
) -> Result<(JoinHandle<()>, String)> {
    let (mut node, node_token) = connect_node(addr, bearer, host_id).await?;
    let handle = tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let reply = if method == "host.resources" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "resources": { "cpuPct": 1, "memPct": 1 } }
                })
            } else {
                let params = frame.get("params").cloned().unwrap_or(json!({}));
                let command_id = params.get("commandId").cloned().unwrap_or(Value::Null);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "command": {
                            "commandId": command_id,
                            "state": "accepted",
                            "operation": method
                        }
                    }
                })
            };
            if node
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok((handle, node_token))
}

/// Poll the host's `online` flag until it equals `want` (the host row itself
/// persists across disconnects, so presence in the list is not enough).
async fn wait_host_present(addr: std::net::SocketAddr, cookie: &str, host_id: &str, want: bool) {
    for _ in 0..100 {
        let (status, _, body) = http(addr, "GET", "/v1/hosts", &[("Cookie", cookie)], None)
            .await
            .expect("host list");
        assert_eq!(status, 200);
        let online = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
            v["items"]
                .as_array()
                .and_then(|items| items.iter().find(|h| h["hostId"] == host_id))
                .and_then(|h| h["online"].as_bool())
        });
        if online == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("host {host_id} never reached online={want}");
}

/// D-055 round 2, item 1: a `instance.configure` refused with NODE_BUSY
/// (pre-send overload) is a TERMINAL rejection carrying the 503. A same-id
/// replay must reproduce that exact 503 status and body — never a 200
/// `replayed` success — and the row reads back as a forwarded, rejected
/// settlement.
#[tokio::test]
async fn saturated_configure_refuses_503_and_replay_reproduces_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let token = enroll_token(hub.addr, &cookie).await?;

    // Bring up an accepting node long enough to create an instance.
    let (create_link, node_token) = accepting_create_node(hub.addr, &token, &host_id).await?;
    wait_host_present(hub.addr, &cookie, &host_id, true).await;
    let create = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "prompt": "hi"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let instance_id = serde_json::from_str::<Value>(body.trim())?["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    create_link.abort();
    wait_host_present(hub.addr, &cookie, &host_id, false).await;

    // Swap in a parked link (reconnecting with the minted node token) and fill
    // every control slot so the next RPC is refused before the frame queues.
    let (_park, node_frames) = park_node(hub.addr, &node_token, &host_id);
    wait_host_present(hub.addr, &cookie, &host_id, true).await;
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
    tokio::time::timeout(Duration::from_secs(10), async {
        while node_frames.load(Ordering::SeqCst) < 32 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("fake Node never received the 32 parked control frames");

    let path = format!("/v1/instances/{instance_id}/commands");
    let command_id = remuda_protocol::CommandId::new();
    let configure = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.configure",
        "payload": { "model": "opus" }
    })
    .to_string();

    // First attempt: refused pre-send with 503 NODE_BUSY.
    let (status, _, first) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&configure),
    )
    .await?;
    assert_eq!(status, 503, "first configure body: {first}");
    let first_body: Value = serde_json::from_str(first.trim())?;
    assert_eq!(first_body["code"], json!("NODE_BUSY"), "{first_body}");
    assert_eq!(first_body["retryable"], json!(true), "{first_body}");
    assert!(
        first_body["retryAfterMs"].as_u64().is_some_and(|ms| ms > 0),
        "{first_body}"
    );

    // Same-id replay reproduces the SAME 503 status and body, never a success.
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&configure),
    )
    .await?;
    assert_eq!(status, 503, "a replay must reproduce the 503: {replay}");
    let replay_body: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(
        replay_body, first_body,
        "the replay body equals the original 503"
    );

    // The row is a terminal, forwarded rejection — not reconciling, not queued.
    let detail = format!("{path}/{}", command_id.as_id().as_str());
    let (status, _, row) = http(hub.addr, "GET", &detail, &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200, "{row}");
    let row: Value = serde_json::from_str(row.trim())?;
    assert_eq!(row["state"], json!("settled"), "{row}");
    assert_eq!(row["forwarded"], json!(true), "{row}");
    assert_eq!(row["settlement"]["outcome"], json!("rejected"), "{row}");

    for handle in parked {
        handle.abort();
    }
    Ok(())
}
