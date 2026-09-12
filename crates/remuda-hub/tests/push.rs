//! Device pairing, `/push` auth, and journal-driven Web Push with a fake endpoint.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn_with_push};
use remuda_protocol::{HostId, InstanceId};
use remuda_push::{Error as PushError, Transport};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

struct FakePush {
    hits: Mutex<Vec<String>>,
}

impl Transport for FakePush {
    fn post(
        &self,
        url: String,
        _headers: Vec<(String, String)>,
        _body: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u16, PushError>> + Send + '_>>
    {
        self.hits.lock().expect("hits").push(url);
        Box::pin(async { Ok(201) })
    }
}

fn sample_keys() -> (String, String) {
    let (pair, auth) = ece::generate_keypair_and_auth_secret().expect("ece");
    use base64::Engine;
    let p256dh =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair.pub_as_raw().expect("pub"));
    let auth_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth);
    (p256dh, auth_b64)
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
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            return Some(
                line.split_once(':')?
                    .1
                    .trim()
                    .split(';')
                    .next()?
                    .trim()
                    .to_string(),
            );
        }
    }
    None
}

/// Mint a single-use Node enroll token with a paired device's cookie (D-018).
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
    let value: Value = serde_json::from_str(rest.trim())?;
    value["token"]
        .as_str()
        .map(str::to_string)
        .context("enroll token")
}

async fn login(
    addr: std::net::SocketAddr,
    bootstrap: &str,
    name: &str,
) -> Result<(String, String)> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": name }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    let cookie = cookie_from(&head).context("cookie")?;
    let json: Value = serde_json::from_str(rest.trim())?;
    let id = json
        .get("deviceId")
        .and_then(Value::as_str)
        .context("deviceId")?
        .to_string();
    Ok((cookie, id))
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
            other => return Err(anyhow!("unexpected {other:?}")),
        }
    }
}

async fn connect_node(
    addr: std::net::SocketAddr,
    enroll: &str,
    host_id: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": { "hostId": host_id, "nodeVersion": "0.1.0-test", "label": "push-node" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["hostId"] == host_id, "{hello}");
    Ok(node)
}

async fn append(
    node: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    instance_id: &str,
    event: Value,
) -> Result<()> {
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "j",
            "method": "journal.append",
            "params": { "instanceId": instance_id, "event": event }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let reply = recv_json(node).await?;
    anyhow::ensure!(reply.get("result").is_some(), "{reply}");
    Ok(())
}

#[tokio::test]
async fn pairing_list_revoke_and_push_with_follow_suppress() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fake = Arc::new(FakePush {
        hits: Mutex::new(Vec::new()),
    });
    let config = HubConfig::for_test(dir.path().join("data"));
    let hub = spawn_with_push(config, fake.clone()).await?;
    let (cookie, desktop_id) = login(hub.addr, &hub.bootstrap_token, "desktop").await?;
    let auth = [("Cookie", cookie.as_str())];

    let (status, _, _) = http(hub.addr, "GET", "/push/config", &[], None).await?;
    assert_eq!(status, 401);

    let (status, _, cfg) = http(hub.addr, "GET", "/push/config", &auth, None).await?;
    assert_eq!(status, 200, "{cfg}");
    let cfg: Value = serde_json::from_str(cfg.trim())?;
    assert!(cfg["public_key"].as_str().unwrap_or("").len() > 8);

    let (status, _, issued) =
        http(hub.addr, "POST", "/v1/devices/pair-code", &auth, Some("{}")).await?;
    assert_eq!(status, 200, "{issued}");
    let issued: Value = serde_json::from_str(issued.trim())?;
    let code = issued["code"].as_str().context("code")?;

    let redeem = json!({ "code": code, "deviceName": "phone" }).to_string();
    let (status, head, rest) =
        http(hub.addr, "POST", "/v1/devices/pair", &[], Some(&redeem)).await?;
    assert_eq!(status, 200, "{rest}");
    assert!(cookie_from(&head).is_some());
    let phone: Value = serde_json::from_str(rest.trim())?;
    let phone_id = phone["deviceId"].as_str().context("phone id")?.to_string();
    assert_ne!(phone_id, desktop_id);

    let (status, _, list) = http(hub.addr, "GET", "/v1/devices", &auth, None).await?;
    assert_eq!(status, 200, "{list}");
    let list: Value = serde_json::from_str(list.trim())?;
    assert_eq!(list["items"].as_array().map(|a| a.len()), Some(2));

    let (status, _, _) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/devices/{phone_id}"),
        &auth,
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let (status, _, list) = http(hub.addr, "GET", "/v1/devices", &auth, None).await?;
    assert_eq!(status, 200, "{list}");
    let list: Value = serde_json::from_str(list.trim())?;
    assert_eq!(list["items"].as_array().map(|a| a.len()), Some(1));

    let (p256dh, auth_key) = sample_keys();
    let sub = json!({
        "endpoint": "http://127.0.0.1:9/fake-push",
        "keys": { "p256dh": p256dh, "auth": auth_key }
    })
    .to_string();
    let (status, _, body) =
        http(hub.addr, "POST", "/push/subscriptions", &auth, Some(&sub)).await?;
    assert_eq!(status, 201, "{body}");

    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let enroll = enroll_token(hub.addr, &cookie).await?;
    let mut node = connect_node(hub.addr, &enroll, host_id.as_id().as_str()).await?;
    append(
        &mut node,
        instance_id.as_id().as_str(),
        json!({
            "kind": "interaction.requested",
            "interactionId": "int_test_1",
            "payload": { "interactionId": "int_test_1" }
        }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_first = fake.hits.lock().expect("hits").len();
    assert!(after_first >= 1, "expected push for interaction.requested");

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
    let _ = recv_json(&mut follow).await?;

    append(
        &mut node,
        instance_id.as_id().as_str(),
        json!({
            "kind": "turn_done",
            "isError": true
        }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_follow = fake.hits.lock().expect("hits").len();
    assert_eq!(
        after_follow, after_first,
        "Paseo suppress: following device must not be pushed"
    );

    drop(follow);
    tokio::time::sleep(Duration::from_millis(50)).await;
    append(
        &mut node,
        instance_id.as_id().as_str(),
        json!({ "kind": "lifecycle", "lifecycle": "exited" }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_exit = fake.hits.lock().expect("hits").len();
    assert!(
        after_exit > after_follow,
        "lifecycle exited should push once follow ends"
    );

    append(
        &mut node,
        instance_id.as_id().as_str(),
        json!({ "kind": "activity", "activity": "waiting-interaction" }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after_block = fake.hits.lock().expect("hits").len();
    assert!(
        after_block > after_exit,
        "blocked > push_block_ms should notify"
    );
    Ok(())
}
