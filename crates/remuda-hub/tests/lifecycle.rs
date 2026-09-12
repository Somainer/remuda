//! Hub derives instance lifecycle from replayed Node journal observations.

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
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "lifecycle-phone"
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

async fn append(
    node: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    rpc_id: &str,
    instance_id: &str,
    event: Value,
) -> Result<()> {
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "journal.append",
            "params": { "instanceId": instance_id, "event": event }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let ack = recv_json(node).await?;
    anyhow::ensure!(ack.get("result").is_some(), "{ack}");
    Ok(())
}

async fn get_instance(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
) -> Result<Value> {
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "get instance {status} {body}");
    Ok(serde_json::from_str(body.trim())?)
}

#[tokio::test]
async fn journal_replay_derives_lifecycle_and_herdr_idle() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let config = HubConfig::for_test(data_dir.clone());
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;

    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    let ins = instance_id.as_id().as_str();
    append(
        &mut node,
        "s1",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "starting", "reasonCode": "driver-start" }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("starting"));
    assert_eq!(
        view["activity"],
        json!("unknown"),
        "create/start default must not look idle"
    );

    append(
        &mut node,
        "s2",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "ready", "reasonCode": "driver-started" }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("running"));
    assert_eq!(
        view["activity"],
        json!("unknown"),
        "ready/running is not herdr idle proof"
    );

    let (status, _, listed) = http(
        addr,
        "GET",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    assert_eq!(listed["items"][0]["lifecycle"], json!("running"));
    assert_eq!(listed["items"][0]["activity"], json!("unknown"));

    append(
        &mut node,
        "s3",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "working" }
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("running"));
    assert_eq!(view["activity"], json!("working"));

    append(
        &mut node,
        "s4",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "blocked" }
            }
        }),
    )
    .await?;
    assert_eq!(
        get_instance(addr, &cookie, ins).await?["activity"],
        json!("blocked")
    );

    append(
        &mut node,
        "s5",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "idle" }
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("running"));
    assert_eq!(view["activity"], json!("idle"));

    append(
        &mut node,
        "s6",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "exited", "reasonCode": "explicit-close" }
        }),
    )
    .await?;
    assert_eq!(
        get_instance(addr, &cookie, ins).await?["lifecycle"],
        json!("exited")
    );

    let failed_id = InstanceId::new();
    append(
        &mut node,
        "f1",
        failed_id.as_id().as_str(),
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity",
                "state": "failed",
                "reasonCode": "native-driver-start-failed"
            }
        }),
    )
    .await?;
    assert_eq!(
        get_instance(addr, &cookie, failed_id.as_id().as_str()).await?["lifecycle"],
        json!("failed")
    );

    drop(node);
    hub.shutdown().await;
    let mut config = HubConfig::for_test(data_dir);
    config.bootstrap_token = bootstrap;
    let hub = spawn(config).await?;
    let view = get_instance(hub.addr, &cookie, ins).await?;
    assert_eq!(
        view["lifecycle"],
        json!("exited"),
        "lifecycle must survive Hub restart"
    );
    assert_eq!(view["activity"], json!("idle"));
    Ok(())
}
