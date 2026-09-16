//! D-027 carrier path: `object.pull` over the node WebSocket (the same frames
//! the ssh-stdio bridge forwards).
//!
//! Covers the authorized inline pull, the streamed (`object.chunk`) path for an
//! object larger than one frame, SHA-256 reassembly, and the cross-host /
//! wrong-instance refusals that keep a host token from reading another host's
//! staged bytes.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(20);

async fn recv_json(
    ws: &mut NodeSocket,
) -> Result<Value> {
    loop {
        let message = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected frame {other:?}")),
        }
    }
}

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Option<&[u8]>,
) -> Result<(u16, String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n"
    );
    if let Some(bytes) = body {
        if let Some(content_type) = content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if let Some(bytes) = body {
        stream.write_all(bytes).await?;
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("no header terminator"))?;
    let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Ok((
        status,
        head_text,
        String::from_utf8_lossy(&buf[split + 4..]).to_string(),
    ))
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
    let (status, head, body) = http(
        addr,
        "POST",
        "/v1/login",
        &[],
        Some("application/json"),
        Some(
            json!({"bootstrapToken": bootstrap, "deviceName": "pull-phone", "deviceKind": "human"})
                .to_string()
                .as_bytes(),
        ),
    )
    .await?;
    anyhow::ensure!(status == 200, "{status} {body}");
    cookie_from(&head).context("login set-cookie")
}

async fn enroll_token(addr: std::net::SocketAddr, cookie: &str) -> Result<String> {
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", cookie)],
        Some("application/json"),
        Some(b"{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "enroll-token {status} {body}");
    let value: Value = serde_json::from_str(&body)?;
    Ok(value["token"].as_str().context("enroll token")?.to_owned())
}

struct Fixture {
    addr: std::net::SocketAddr,
    cookie: String,
    node: NodeSocket,
    instance_id: String,
    _hub: remuda_hub::RunningHub,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let enroll = enroll_token(addr, &cookie).await?;

    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    let host_id = HostId::new().as_id().as_str().to_owned();
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.1.0", "label":"pull-node"}})
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.pointer("/result/hostId").is_some(), "{hello}");

    // Project a running instance onto this host so the upload binds the object
    // to host_id + instance_id.
    let instance_id = InstanceId::new().as_id().as_str().to_owned();
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"s1", "method":"journal.append",
            "params":{"instanceId":instance_id, "event":{
                "kind":"lifecycle",
                "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                           "reasonCode":"driver-started"}}}})
        .to_string()
        .into(),
    ))
    .await?;
    let ack = recv_json(&mut node).await?;
    anyhow::ensure!(ack["id"] == "s1" && ack.get("result").is_some(), "{ack}");

    Ok(Fixture {
        addr,
        cookie,
        node,
        instance_id,
        _hub: hub,
        _dir: dir,
    })
}

async fn upload(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
    bytes: &[u8],
) -> Result<Value> {
    let (status, _, body) = http(
        addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", cookie)],
        Some("application/octet-stream"),
        Some(bytes),
    )
    .await?;
    anyhow::ensure!(status == 200, "upload {status} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// Send object.pull and collect the metadata reply plus any object.chunk
/// notifications the Hub streams first, in arrival order.
async fn pull(
    node: &mut NodeSocket,
    rpc_id: &str,
    object_id: &str,
    instance_id: &str,
) -> Result<(Value, Vec<Value>)> {
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":rpc_id, "method":"object.pull",
            "params":{"objectId":object_id, "instanceId":instance_id}})
        .to_string()
        .into(),
    ))
    .await?;
    let mut chunks = Vec::new();
    loop {
        let frame = recv_json(node).await?;
        if frame.get("method").and_then(Value::as_str) == Some("object.chunk") {
            chunks.push(frame["params"].clone());
            continue;
        }
        if frame["id"] == rpc_id {
            return Ok((frame, chunks));
        }
    }
}

fn decode(value: &Value) -> Result<Vec<u8>> {
    use base64::Engine;
    let raw = value
        .as_str()
        .ok_or_else(|| anyhow!("chunk dataBase64 missing"))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(raw)?)
}

#[tokio::test]
async fn authorized_pull_returns_the_object_inline() -> Result<()> {
    let mut fixture = fixture().await?;
    let bytes = b"brief: ship the carrier pull\n".to_vec();
    let object = upload(
        fixture.addr,
        &fixture.cookie,
        &fixture.instance_id,
        &bytes,
    )
    .await?;
    let object_id = object["objectId"].as_str().unwrap();

    let (reply, chunks) = pull(
        &mut fixture.node,
        "pull-1",
        object_id,
        &fixture.instance_id,
    )
    .await?;
    assert!(chunks.is_empty(), "a small object must not be chunked: {reply}");
    let result = reply
        .get("result")
        .with_context(|| format!("expected result, got {reply}"))?;
    assert_eq!(result["objectId"], object_id);
    assert_eq!(result["mime"], "application/octet-stream");
    assert_eq!(result["size"], bytes.len());
    let data = decode(&result["dataBase64"])?;
    assert_eq!(data, bytes);
    let actual = format!("{:x}", sha2::Sha256::digest(&data));
    assert_eq!(result["sha256"], actual);
    Ok(())
}

#[tokio::test]
async fn large_object_is_streamed_as_chunks_and_reassembled() -> Result<()> {
    let mut fixture = fixture().await?;
    // 900 KiB exceeds the ~768 KiB inline ceiling but stays under the 25 MiB
    // attachment cap, so the Hub must stream it.
    let bytes: Vec<u8> = (0..900 * 1024u32).map(|n| (n % 251) as u8).collect();
    let object = upload(
        fixture.addr,
        &fixture.cookie,
        &fixture.instance_id,
        &bytes,
    )
    .await?;
    let object_id = object["objectId"].as_str().unwrap();

    let (reply, chunks) = pull(
        &mut fixture.node,
        "pull-big",
        object_id,
        &fixture.instance_id,
    )
    .await?;
    assert!(chunks.len() >= 2, "expected streamed chunks, got {chunks:?}");
    assert!(
        reply["result"].get("dataBase64").is_none(),
        "a streamed reply carries metadata only: {reply}"
    );
    let mut reassembled = Vec::with_capacity(bytes.len());
    for (position, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk["objectId"], object_id);
        assert_eq!(chunk["seq"], position as u64, "chunks must be contiguous");
        reassembled.extend_from_slice(&decode(&chunk["dataBase64"])?);
    }
    assert_eq!(reassembled, bytes);
    assert_eq!(reply["result"]["size"], bytes.len());
    let actual = format!("{:x}", sha2::Sha256::digest(&reassembled));
    assert_eq!(reply["result"]["sha256"], actual);
    Ok(())
}

#[tokio::test]
async fn pull_is_refused_for_another_host_or_instance() -> Result<()> {
    let mut fixture = fixture().await?;
    let bytes = b"host-scoped bytes\n".to_vec();
    let object = upload(
        fixture.addr,
        &fixture.cookie,
        &fixture.instance_id,
        &bytes,
    )
    .await?;
    let object_id = object["objectId"].as_str().unwrap();

    // Same host, wrong staging instance: the instance is part of the grant.
    let (reply, _) = pull(
        &mut fixture.node,
        "pull-wrong-instance",
        object_id,
        InstanceId::new().as_id().as_str(),
    )
    .await?;
    assert_eq!(reply["error"]["code"], -32001, "{reply}");

    // A second, different host must not read the first host's object: pair a
    // fresh human session and enroll host B with its own token.
    let token_value = login(fixture.addr, &fixture._hub.bootstrap_token).await?;
    let token = enroll_token(fixture.addr, &token_value).await?;
    let mut request = format!("ws://{}/v1/node", fixture.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);
    let (mut other, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    let other_host = HostId::new().as_id().as_str().to_owned();
    other.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"hello-b", "method":"node.hello",
            "params":{"hostId": other_host, "nodeVersion":"0.1.0", "label":"pull-node-b"}})
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut other).await?;
    anyhow::ensure!(hello.get("result").is_some(), "{hello}");

    let (reply, _) = pull(
        &mut other,
        "pull-cross-host",
        object_id,
        &fixture.instance_id,
    )
    .await?;
    assert_eq!(reply["error"]["code"], -32001, "cross-host: {reply}");

    // An unknown object id reads as not found.
    let (reply, _) = pull(
        &mut fixture.node,
        "pull-missing",
        "obj_does_not_exist",
        &fixture.instance_id,
    )
    .await?;
    assert_eq!(reply["error"]["code"], -32601, "{reply}");
    Ok(())
}

#[tokio::test]
async fn pull_enforces_the_hub_attachment_cap() -> Result<()> {
    // Upload under the default cap, then reopen the same store with a smaller
    // configured cap: object.pull must reject bytes the Node would otherwise
    // pull past the Hub's configured maximum (defence in depth alongside the
    // upload-time check).
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let bootstrap;
    let node_token;
    let object_id;
    let host_id;
    let instance_id;
    {
        let config = HubConfig::for_test(data_dir.clone());
        bootstrap = config.bootstrap_token.clone();
        let hub = spawn(config).await?;
        let cookie = login(hub.addr, &bootstrap).await?;
        let enroll = enroll_token(hub.addr, &cookie).await?;
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut node, _) =
            tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
        host_id = HostId::new().as_id().as_str().to_owned();
        node.send(Message::Text(
            json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params":{"hostId": host_id, "label":"cap-node"}})
            .to_string()
            .into(),
        ))
        .await?;
        let hello = recv_json(&mut node).await?;
        node_token = hello
            .pointer("/result/nodeToken")
            .and_then(Value::as_str)
            .context("node token")?
            .to_owned();
        instance_id = InstanceId::new().as_id().as_str().to_owned();
        node.send(Message::Text(
            json!({"jsonrpc":"2.0", "id":"s1", "method":"journal.append",
                "params":{"instanceId":instance_id, "event":{
                    "kind":"lifecycle",
                    "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                               "reasonCode":"driver-started"}}}})
            .to_string()
            .into(),
        ))
        .await?;
        let ack = recv_json(&mut node).await?;
        anyhow::ensure!(ack["id"] == "s1" && ack.get("result").is_some(), "{ack}");
        let bytes = vec![0x42u8; 4096];
        let object = upload(hub.addr, &cookie, &instance_id, &bytes).await?;
        object_id = object["objectId"].as_str().unwrap().to_owned();
        hub.shutdown().await;
    }

    let mut config = HubConfig::for_test(data_dir);
    config.bootstrap_token = bootstrap.clone();
    config.attachment_max_bytes = 1024;
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let _enroll = enroll_token(hub.addr, &cookie).await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {node_token}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"hello2", "method":"node.hello",
            "params":{"hostId": host_id, "label":"cap-node"}})
        .to_string()
        .into(),
    ))
    .await?;
    anyhow::ensure!(recv_json(&mut node).await?.get("result").is_some());

    let (reply, chunks) = pull(&mut node, "pull-cap", &object_id, &instance_id).await?;
    assert!(chunks.is_empty());
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("RESOURCE_LIMIT"),
        "{reply}"
    );
    Ok(())
}
