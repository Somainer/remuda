//! Drive Hub HTTP + `/v1/node` + `/v1/follow` with a fake Node client.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let hub = spawn(config).await?;
    let token = hub.bootstrap_token.clone();
    Ok((hub, token, dir))
}

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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<(String, String)> {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "test-phone"
    })
    .to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    let cookie = cookie_from(&head).context("set-cookie")?;
    let json: Value = serde_json::from_str(rest.trim())?;
    let token = json
        .get("token")
        .and_then(Value::as_str)
        .context("device token")?
        .to_string();
    Ok((cookie, token))
}

#[tokio::test]
async fn healthz_ok() -> Result<()> {
    let (hub, _, _dir) = boot().await?;
    let (status, _, body) = http(hub.addr, "GET", "/healthz", &[], None).await?;
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true") || body.contains("\"ok\": true"));
    Ok(())
}

#[tokio::test]
async fn auth_reject_http_and_origin() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (status, _, _) = http(hub.addr, "GET", "/v1/hosts", &[], None).await?;
    assert_eq!(status, 401);

    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "x" }).to_string();
    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/login",
        &[("Origin", "http://evil.example")],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 403);

    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/login",
        &[],
        Some(&json!({"bootstrapToken":"nope","deviceName":"x"}).to_string()),
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

#[tokio::test]
async fn node_ws_rejects_missing_token() -> Result<()> {
    let (hub, _, _dir) = boot().await?;
    let url = format!("ws://{}/v1/node", hub.addr);
    let err = tokio_tungstenite::connect_async(url).await.err();
    assert!(err.is_some(), "unauthenticated node socket must fail");
    Ok(())
}

#[tokio::test]
async fn fake_node_hello_heartbeat_append_then_http_and_follow() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _) = login(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect timeout")??;

    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0-test",
                "label": "fake-node"
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    assert_eq!(hello["id"], "1");
    assert!(hello["result"]["nodeToken"].as_str().is_some());
    assert_eq!(hello["result"]["protocol"]["major"], json!(1));

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "node.heartbeat",
            "params": {
                "cli": [{
                    "kind": "claude",
                    "version": "2.1.268",
                    "path": "/usr/bin/claude",
                    "auth": "unknown"
                }]
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let beat = recv_json(&mut node).await?;
    assert_eq!(beat["id"], "2");
    assert!(beat.get("result").is_some());

    let event: Value = serde_json::from_str(include_str!("fixtures/journal-event.json"))?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": event
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let appended = recv_json(&mut node).await?;
    assert_eq!(appended["result"]["seq"], json!("1"));

    let (status, _, hosts) =
        http(hub.addr, "GET", "/v1/hosts", &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200);
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    assert_eq!(hosts["items"][0]["online"], json!(true));
    assert_eq!(hosts["items"][0]["cli"][0]["kind"], json!("claude"));

    let journal_path = format!("/v1/instances/{}/journal", instance_id.as_id().as_str());
    let (status, _, journal) =
        http(hub.addr, "GET", &journal_path, &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200, "{journal}");
    let journal: Value = serde_json::from_str(journal.trim())?;
    assert_eq!(journal["durableSeq"], json!("1"));
    assert!(
        journal["events"][0]["event"]["payload"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("fake-node")
    );

    let mut follow_req = format!(
        "ws://{}/v1/follow?instanceId={}",
        hub.addr,
        instance_id.as_id().as_str()
    )
    .into_client_request()?;
    follow_req
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req))
            .await
            .context("follow connect")??;
    let snapshot = recv_json(&mut follow).await?;
    assert_eq!(snapshot["type"], json!("snapshot"));
    assert_eq!(snapshot["asOfSeq"], json!("1"));
    assert_eq!(snapshot["events"].as_array().map(|a| a.len()), Some(1));

    // Second append should arrive as a live follow event.
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "4",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": { "kind": "message", "payload": { "text": "second" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    let live = recv_json(&mut follow).await?;
    assert_eq!(live["type"], json!("event"));
    assert_eq!(live["seq"], json!("2"));
    Ok(())
}

#[tokio::test]
async fn command_stays_queued_when_node_offline_and_is_not_resent() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _) = login(hub.addr, &bootstrap).await?;

    // Enroll a host then drop the socket so it is offline.
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .context("nodeToken")?
        .to_string();
    node.close(None).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
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
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["command"]["state"], json!("queued"));
    assert_eq!(body["command"]["forwarded"], json!(false));
    let command_id = body["command"]["commandId"].as_str().unwrap().to_string();
    let instance_id = body["instance"]["instanceId"].as_str().unwrap().to_string();

    // Reconnect: Hub must not auto-resend the queued command.
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h2",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str() }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    let raced = tokio::time::timeout(Duration::from_millis(250), recv_json(&mut node)).await;
    assert!(raced.is_err(), "reconnect must not replay commands");

    // Same commandId + payload is idempotent (reconnect must not create a second native send).
    let replay = json!({
        "commandId": command_id,
        "operation": "instance.create",
        "payload": body["command"]["payload"]
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&replay),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["replayed"], json!(true));
    Ok(())
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
