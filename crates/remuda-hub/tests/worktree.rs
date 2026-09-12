//! Hub `/v1/worktrees` forwards to a connected Node.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            let token = value.split(';').next()?.trim();
            return Some(token.to_string());
        }
    }
    None
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "worktree-phone"
    })
    .to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    cookie_from(&head).context("set-cookie")
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(TIMEOUT, ws.next())
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

#[tokio::test]
async fn worktree_list_and_create_forward_to_node() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", hub.bootstrap_token).parse().unwrap(),
    );
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect timeout")??;

    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0-test",
                "label": "wt-node",
                "host": { "hostname": "wt-node.local" }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.get("result").is_some(), "{hello}");

    let (node_tx, node_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if frame.get("method").and_then(Value::as_str) == Some("worktree.list") {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                let _ = node
                    .send(Message::Text(
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "workspaceRoot": "/tmp/repo",
                                "items": [{
                                    "name": "existing",
                                    "path": "/tmp/repo-wt/existing",
                                    "branch": "wt/existing/work",
                                    "base": "main"
                                }]
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                continue;
            }
            if frame.get("method").and_then(Value::as_str) == Some("worktree.create") {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                let name = frame["params"]["name"].as_str().unwrap_or("agent");
                let _ = node
                    .send(Message::Text(
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "name": name,
                                "path": format!("/tmp/repo-wt/{name}"),
                                "branch": format!("wt/{name}/work"),
                                "base": "main"
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                continue;
            }
            let _ = node_tx.send(frame);
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, _, listed) = http(
        hub.addr,
        "GET",
        "/v1/worktrees",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    assert_eq!(listed["items"][0]["name"], json!("existing"));
    assert_eq!(listed["workspaceRoot"], json!("/tmp/repo"));

    let body = json!({ "name": "agent1", "base": "main" }).to_string();
    let (status, _, created) = http(
        hub.addr,
        "POST",
        "/v1/worktrees",
        &[("Cookie", &cookie)],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 200, "{created}");
    let created: Value = serde_json::from_str(created.trim())?;
    assert_eq!(created["name"], json!("agent1"));
    assert_eq!(created["path"], json!("/tmp/repo-wt/agent1"));
    assert!(
        created["branch"]
            .as_str()
            .unwrap()
            .starts_with("wt/agent1/")
    );

    drop(node_rx);
    Ok(())
}
