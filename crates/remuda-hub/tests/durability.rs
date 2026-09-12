//! Hub process restart: host id, journal watermark, pending interactions, device cookie.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId, InteractionId};
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
        "deviceName": "durability-phone"
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

async fn node_hello(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Value,
)> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": { "hostId": host_id, "nodeVersion": "0.1.0-durability" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    Ok((node, hello))
}

#[tokio::test]
async fn hub_restart_keeps_host_journal_interactions_and_device() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let config = HubConfig::for_test(data_dir.clone());
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let interaction_id = InteractionId::new();

    let (mut node, hello) = node_hello(addr, &bootstrap, host_id.as_id().as_str()).await?;
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .context("nodeToken")?
        .to_string();
    assert_eq!(
        hello["result"]["hostId"].as_str(),
        Some(host_id.as_id().as_str())
    );

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "a1",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "seq": "1",
                "event": { "kind": "message", "payload": { "text": "one" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let appended = recv_json(&mut node).await?;
    assert_eq!(appended["result"]["seq"], json!("1"));

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "a2",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "seq": "2",
                "event": {
                    "kind": "interaction.requested",
                    "interactionId": interaction_id.as_id().as_str(),
                    "payload": {
                        "interactionId": interaction_id.as_id().as_str(),
                        "kind": "permission"
                    }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    let (status, _, pending) = http(
        addr,
        "GET",
        "/v1/interactions",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{pending}");
    let pending: Value = serde_json::from_str(pending.trim())?;
    assert_eq!(pending["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        pending["items"][0]["interactionId"].as_str(),
        Some(interaction_id.as_id().as_str())
    );

    drop(node);
    hub.shutdown().await;

    let mut config = HubConfig::for_test(data_dir);
    config.bootstrap_token = bootstrap.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;

    let (status, _, hosts) = http(
        addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "device cookie must survive restart: {hosts}");
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    assert_eq!(
        hosts["items"][0]["hostId"].as_str(),
        Some(host_id.as_id().as_str())
    );

    let (status, _, pending) = http(
        addr,
        "GET",
        "/v1/interactions",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{pending}");
    let pending: Value = serde_json::from_str(pending.trim())?;
    assert_eq!(
        pending["items"].as_array().map(Vec::len),
        Some(1),
        "pending interaction must survive without a connected Node"
    );
    assert_eq!(pending["items"][0]["state"], json!("pending"));

    let (mut node, hello) = node_hello(addr, &node_token, host_id.as_id().as_str()).await?;
    assert!(
        hello["result"]["nodeToken"].is_null() || hello["result"].get("nodeToken").is_none(),
        "re-enroll must not mint a second host token"
    );
    assert_eq!(
        hello["result"]["hostId"].as_str(),
        Some(host_id.as_id().as_str())
    );
    let marks = hello["result"]["instanceWatermarks"]
        .as_array()
        .context("instanceWatermarks")?;
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0]["durableSeq"], json!("2"));
    assert_eq!(
        marks[0]["instanceId"].as_str(),
        Some(instance_id.as_id().as_str())
    );

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "replay",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "seq": "1",
                "event": { "kind": "message", "payload": { "text": "one" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let replayed = recv_json(&mut node).await?;
    assert_eq!(replayed["result"]["seq"], json!("1"));
    assert_eq!(replayed["result"]["replayed"], json!(true));
    assert_eq!(
        replayed["result"]["durableSeq"],
        json!("2"),
        "replay ACK must keep the instance watermark"
    );

    let journal_path = format!("/v1/instances/{}/journal", instance_id.as_id().as_str());
    let (status, _, journal) = http(
        addr,
        "GET",
        &journal_path,
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{journal}");
    let journal: Value = serde_json::from_str(journal.trim())?;
    assert_eq!(
        journal["durableSeq"],
        json!("2"),
        "replay of seq 1 must not rewind or duplicate the watermark"
    );
    assert_eq!(journal["events"].as_array().map(Vec::len), Some(2));

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "a3",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "seq": "3",
                "event": { "kind": "message", "payload": { "text": "three" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let third = recv_json(&mut node).await?;
    assert_eq!(third["result"]["seq"], json!("3"));
    let (status, _, journal) = http(
        addr,
        "GET",
        &journal_path,
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let journal: Value = serde_json::from_str(journal.trim())?;
    assert_eq!(journal["durableSeq"], json!("3"));
    assert_eq!(journal["events"].as_array().map(Vec::len), Some(3));
    Ok(())
}

#[tokio::test]
async fn follow_backpressure_does_not_duplicate_journal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.follow_buffer_events = 1;
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let (mut node, _) = node_hello(hub.addr, &bootstrap, host_id.as_id().as_str()).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "seed",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": { "kind": "message", "payload": { "text": "seed" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

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
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req)).await??;
    let snap = recv_json(&mut follow).await?;
    assert_eq!(snap["type"], json!("snapshot"));

    for i in 0..20 {
        node.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": format!("f{i}"),
                "method": "journal.append",
                "params": {
                    "instanceId": instance_id.as_id().as_str(),
                    "event": { "kind": "message", "payload": { "text": format!("burst-{i}") } }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
        let ack = recv_json(&mut node).await?;
        assert!(ack.get("result").is_some(), "{ack}");
    }

    drop(follow);
    let journal_path = format!("/v1/instances/{}/journal", instance_id.as_id().as_str());
    let (status, _, journal) = http(
        hub.addr,
        "GET",
        &journal_path,
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{journal}");
    let journal: Value = serde_json::from_str(journal.trim())?;
    let events = journal["events"].as_array().context("events")?;
    assert_eq!(events.len(), 21);
    let seqs: Vec<i64> = events
        .iter()
        .filter_map(|ev| {
            ev.get("seq").and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
        })
        .collect();
    assert_eq!(seqs.len(), events.len());
    let mut unique = seqs.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        seqs.len(),
        unique.len(),
        "journal seqs must stay unique under follow backpressure"
    );
    assert_eq!(journal["durableSeq"], json!("21"));
    Ok(())
}
