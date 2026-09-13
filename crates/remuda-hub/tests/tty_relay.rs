//! TTY relay hardening and zombie-instance reconciliation over a live Hub.
//!
//! Covers the failures found on the demo:
//! * B2 — after a Node reconnect, tty output must keep reaching follow sockets
//!   that were already open (the stream binding is per-host, not per-socket).
//! * B3 — when the Node cannot be reached, the Hub's cached snapshot is sent
//!   instead of being computed and dropped.
//! * B4 — every tty input/resize failure produces a `tty.diagnostic` frame.
//! * Zombies — a Node restart reconciles lost instances, a stop for an
//!   instance the Node forgot settles as `exited`, unacknowledged `requested`
//!   rows expire, and none of them keep holding a placement slot.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{BinaryChannel, HostId, InstanceId, StreamUuid, encode_binary_frame};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

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
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':'))
        .and_then(|(_, value)| value.trim().split(';').next())
        .map(|token| token.trim().to_string())
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "tty-relay" }).to_string();
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
    serde_json::from_str::<Value>(rest.trim())?["token"]
        .as_str()
        .map(str::to_string)
        .context("enroll token")
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
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => continue,
            other => return Err(anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

/// Connect a Node socket and complete `node.hello`, optionally announcing an
/// epoch and an instance inventory (what a daemon Node sends on reconnect).
async fn node_hello(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &HostId,
    epoch: Option<&str>,
    instances: Option<Value>,
) -> Result<(Ws, String)> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    let mut params = json!({
        "hostId": host_id.as_id().as_str(),
        "nodeVersion": "0.1.0",
        "label": "tty-relay-node",
    });
    if let Some(epoch) = epoch {
        params["nodeEpoch"] = json!(epoch);
    }
    if let Some(instances) = instances {
        params["instances"] = instances;
    }
    node.send(Message::Text(
        json!({"jsonrpc":"2.0","id":"hello","method":"node.hello","params":params})
            .to_string()
            .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.get("result").is_some(), "{hello}");
    // The enroll token is single-use; a reconnect must present the host token
    // minted on first enrollment, exactly as a real Node does.
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .unwrap_or(bearer)
        .to_string();
    Ok((node, node_token))
}

/// Seed an instance row on `host_id` by appending one Node journal event.
async fn seed_instance(node: &mut Ws, instance_id: &InstanceId, rpc_id: &str) -> Result<()> {
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": {
                    "kind": "lifecycle",
                    "payload": { "type": "entity", "entityType": "instance", "state": "ready" }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let ack = recv_json(node).await?;
    anyhow::ensure!(ack.get("result").is_some(), "{ack}");
    Ok(())
}

/// Register a tty stream for an instance via a JSON `tty.frame` announcement.
async fn bind_stream(
    node: &mut Ws,
    instance_id: &InstanceId,
    stream_id: &remuda_protocol::Id,
    rpc_id: &str,
) -> Result<()> {
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "tty.frame",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "streamId": stream_id.as_str(),
                "channel": 1
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let ack = recv_json(node).await?;
    anyhow::ensure!(ack["result"]["ok"] == json!(true), "{ack}");
    Ok(())
}

/// Open a follow socket. `tty=1` is requested in-band so the handshake does not
/// immediately issue `tty.attach` against a Node nobody is answering for.
async fn follow_socket(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &InstanceId,
) -> Result<Ws> {
    let mut req = format!(
        "ws://{addr}/v1/follow?instanceId={}",
        instance_id.as_id().as_str()
    )
    .into_client_request()?;
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    let snapshot = recv_json(&mut follow).await?;
    anyhow::ensure!(snapshot["type"] == json!("snapshot"), "{snapshot}");
    follow
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "instanceIds": [instance_id.as_id().as_str()],
                "tty": 1
            })
            .to_string()
            .into(),
        ))
        .await?;
    Ok(follow)
}

/// Read follow frames until one matches, or the deadline passes.
async fn wait_for<F, T>(follow: &mut Ws, mut matcher: F) -> Option<T>
where
    F: FnMut(&Message) -> Option<T>,
{
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), follow.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Some(found) = matcher(&msg) {
                    return Some(found);
                }
            }
            Ok(Some(Err(_)) | None) => return None,
            Err(_) => continue,
        }
    }
    None
}

fn binary_containing<'a>(needle: &'a [u8]) -> impl FnMut(&Message) -> Option<Vec<u8>> + 'a {
    move |msg| match msg {
        Message::Binary(bytes) if bytes.windows(needle.len()).any(|w| w == needle) => {
            Some(bytes.to_vec())
        }
        _ => None,
    }
}

fn json_of_type<'a>(kind: &'a str) -> impl FnMut(&Message) -> Option<Value> + 'a {
    move |msg| match msg {
        Message::Text(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|value| value["type"] == json!(kind)),
        _ => None,
    }
}

/// B2: a Node reconnect must not silently mute an already-open follow socket.
///
/// Before this fix the stream→instance binding was scoped to the *socket* that
/// announced it, so after a reconnect the new socket owned nothing and every
/// binary frame was dropped: the attach snapshot still painted, so the terminal
/// looked alive while being completely dead.
#[tokio::test]
async fn tty_output_resumes_on_an_open_follow_socket_after_a_node_reconnect() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let stream_id = remuda_protocol::Id::new("tty")?;
    let uuid = StreamUuid::from_prefixed_id(stream_id.as_str()).expect("stream uuid");

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, node_token) =
        node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    bind_stream(&mut node, &instance_id, &stream_id, "bind-1").await?;

    let mut follow = follow_socket(hub.addr, &cookie, &instance_id).await?;
    node.send(Message::Binary(
        encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, b"before-restart")?.into(),
    ))
    .await?;
    anyhow::ensure!(
        wait_for(&mut follow, binary_containing(b"before-restart"))
            .await
            .is_some(),
        "baseline tty output never reached the follow socket"
    );

    // Kill the Node socket. The follow socket deliberately stays open.
    node.close(None).await?;
    drop(node);

    // The Node comes back on a new socket and re-announces the same stream,
    // exactly as `TtyEvent::Open` does after a reconnect. It presents the host
    // token from its first enrollment, because enroll tokens are single-use.
    let (mut node, _) = node_hello(
        hub.addr,
        &node_token,
        &host_id,
        Some("epoch_two"),
        Some(json!([{ "id": instance_id.as_id().as_str(), "hostId": host_id.as_id().as_str() }])),
    )
    .await?;
    bind_stream(&mut node, &instance_id, &stream_id, "bind-2").await?;
    node.send(Message::Binary(
        encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, b"after-restart")?.into(),
    ))
    .await?;

    anyhow::ensure!(
        wait_for(&mut follow, binary_containing(b"after-restart"))
            .await
            .is_some(),
        "tty output was dropped after the node reconnected (B2)"
    );
    Ok(())
}

/// B3: with the Node gone, the attach path falls back to the Hub's own cache
/// instead of computing a snapshot and discarding it.
#[tokio::test]
async fn cached_snapshot_is_sent_when_the_node_cannot_be_attached() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let stream_id = remuda_protocol::Id::new("tty")?;
    let uuid = StreamUuid::from_prefixed_id(stream_id.as_str()).expect("stream uuid");

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    bind_stream(&mut node, &instance_id, &stream_id, "bind").await?;
    // A follower has to be watching for the bytes to be observable; the relay
    // caches them on the same path.
    let mut warmup = follow_socket(hub.addr, &cookie, &instance_id).await?;
    node.send(Message::Binary(
        encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, b"cached-screen")?.into(),
    ))
    .await?;
    anyhow::ensure!(
        wait_for(&mut warmup, binary_containing(b"cached-screen"))
            .await
            .is_some(),
        "hub never cached the tty output"
    );
    drop(warmup);
    node.close(None).await?;
    drop(node);

    // A fresh follower attaches with no Node to answer `tty.attach`.
    let mut follow = follow_socket(hub.addr, &cookie, &instance_id).await?;
    let cached = wait_for(&mut follow, json_of_type("tty.snapshot"))
        .await
        .context("hub did not send its cached snapshot (B3)")?;
    assert_eq!(cached["source"], json!("hub-cache"));
    assert_eq!(cached["instanceId"], json!(instance_id.as_id().as_str()));
    let decoded = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(cached["dataBase64"].as_str().context("dataBase64")?)?
    };
    anyhow::ensure!(
        decoded
            .windows(b"cached-screen".len())
            .any(|w| w == b"cached-screen"),
        "cached snapshot did not carry the buffered bytes"
    );
    Ok(())
}

/// B4: an input that cannot reach a PTY must say so instead of vanishing.
#[tokio::test]
async fn failed_tty_input_and_resize_surface_a_diagnostic() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let stream_id = remuda_protocol::Id::new("tty")?;
    let uuid = StreamUuid::from_prefixed_id(stream_id.as_str()).expect("stream uuid");

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    bind_stream(&mut node, &instance_id, &stream_id, "bind").await?;
    let mut follow = follow_socket(hub.addr, &cookie, &instance_id).await?;

    // A frame on the wrong channel is rejected before any Node call.
    follow
        .send(Message::Binary(
            encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, b"wrong-way")?.into(),
        ))
        .await?;
    let wrong_channel = wait_for(&mut follow, json_of_type("tty.diagnostic"))
        .await
        .context("no diagnostic for a wrong-channel input frame")?;
    assert_eq!(wrong_channel["reason"], json!("wrong-channel"));
    assert_eq!(wrong_channel["operation"], json!("tty.write"));

    // A truncated frame cannot even be decoded.
    follow.send(Message::Binary(vec![0u8; 8].into())).await?;
    let malformed = wait_for(&mut follow, json_of_type("tty.diagnostic"))
        .await
        .context("no diagnostic for a malformed input frame")?;
    assert_eq!(malformed["reason"], json!("malformed-frame"));

    // The Node is up but rejects the write: the error must reach the browser.
    let node_task = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_millis(300), node.next()).await
            else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("method").and_then(Value::as_str) == Some("tty.write") {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": value.get("id").cloned().unwrap_or(Value::Null),
                    "error": { "code": -32004, "message": "instance not found: pty is gone" }
                });
                let _ = node.send(Message::Text(reply.to_string().into())).await;
                return;
            }
        }
    });
    follow
        .send(Message::Binary(
            encode_binary_frame(BinaryChannel::TtyInput, uuid, 0, b"ls\r")?.into(),
        ))
        .await?;
    let node_error = wait_for(&mut follow, json_of_type("tty.diagnostic"))
        .await
        .context("no diagnostic for a node-rejected tty.write")?;
    assert_eq!(node_error["reason"], json!("node-error"));
    assert_eq!(
        node_error["instanceId"],
        json!(instance_id.as_id().as_str())
    );
    anyhow::ensure!(
        node_error["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("pty is gone")),
        "{node_error}"
    );
    node_task.await.ok();

    // With the Node gone entirely, a resize reports the offline host.
    follow
        .send(Message::Text(
            json!({ "type": "tty.resize", "cols": 100, "rows": 30 })
                .to_string()
                .into(),
        ))
        .await?;
    let offline = wait_for(&mut follow, json_of_type("tty.diagnostic"))
        .await
        .context("no diagnostic for a resize with no live node")?;
    assert_eq!(offline["operation"], json!("tty.resize"));
    assert_eq!(offline["reason"], json!("node-offline"));
    Ok(())
}

/// Zombie instances: a Node restart must reconcile rows the new process does
/// not know, journal why, and give their placement slots back.
#[tokio::test]
async fn node_restart_reconciles_lost_instances_and_frees_the_cap() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let survivor = InstanceId::new();
    let zombie = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, node_token) =
        node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &survivor, "seed-a").await?;
    seed_instance(&mut node, &zombie, "seed-b").await?;
    node.close(None).await?;
    drop(node);

    // The host cap is 8 by default and both rows are `running`, so the Node's
    // restart is the only thing that can free the zombie's slot.
    let mut follow = follow_socket(hub.addr, &cookie, &zombie).await?;
    let _node = node_hello(
        hub.addr,
        &node_token,
        &host_id,
        Some("epoch_two"),
        Some(
            json!([{ "id": survivor.as_id().as_str(), "hostId": host_id.as_id().as_str(),
                      "lifecycle": "running" }]),
        ),
    )
    .await?;

    let diagnostic = wait_for(&mut follow, |msg| match msg {
        Message::Text(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|value| value["event"]["payload"]["nativeName"] == json!("node_epoch_changed")),
        _ => None,
    })
    .await
    .context("no journal diagnostic for the lost instance")?;
    assert_eq!(diagnostic["event"]["payload"]["origin"], json!("hub"));
    assert_eq!(diagnostic["instanceId"], json!(zombie.as_id().as_str()));

    let (status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{}", zombie.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "{status} {body}");
    let zombie_row: Value = serde_json::from_str(body.trim())?;
    assert_eq!(zombie_row["lifecycle"], json!("exited"));
    assert_eq!(zombie_row["lastError"], json!("node-epoch-changed"));

    let (status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{}", survivor.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "{status} {body}");
    let survivor_row: Value = serde_json::from_str(body.trim())?;
    assert_eq!(
        survivor_row["lifecycle"],
        json!("running"),
        "an instance the node still reports must survive the reconcile"
    );
    Ok(())
}

/// The operator ceiling must survive a Node hello and a Hub restart, and the
/// zombie rows left behind must not come back holding placement slots.
#[tokio::test]
async fn max_instances_override_and_reconciliation_survive_a_hub_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let host_id = HostId::new();
    let stale = InstanceId::new();
    let bootstrap;

    let hub = spawn(HubConfig::for_test(data_dir.clone())).await?;
    bootstrap = hub.bootstrap_token.clone();
    let cookie = login(hub.addr, &bootstrap).await?;
    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, node_token) =
        node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &stale, "seed").await?;

    let (status, _, body) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/hosts/{}", host_id.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        Some(&json!({ "maxInstances": 32 }).to_string()),
    )
    .await?;
    anyhow::ensure!(status == 200, "patch host {status} {body}");
    assert_eq!(
        serde_json::from_str::<Value>(body.trim())?["maxInstances"],
        json!(32)
    );

    // A Node heartbeat re-advertises the Node's own ceiling; it must lose.
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hb",
            "method": "runtime.heartbeat",
            "params": { "maxInstances": 8 }
        })
        .to_string()
        .into(),
    ))
    .await?;
    anyhow::ensure!(recv_json(&mut node).await?.get("result").is_some());
    node.close(None).await?;
    drop(node);
    drop(hub);

    // Restart the Hub against the same data dir.
    let mut config = HubConfig::for_test(data_dir);
    config.bootstrap_token = bootstrap.clone();
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let (status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/hosts/{}", host_id.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "{status} {body}");
    assert_eq!(
        serde_json::from_str::<Value>(body.trim())?["maxInstances"],
        json!(32),
        "the operator ceiling was reset by the restart"
    );

    // The row survived the restart as `running`; reconnecting on a new epoch
    // without it in the inventory reconciles it away.
    let _node = node_hello(
        hub.addr,
        &node_token,
        &host_id,
        Some("epoch_three"),
        Some(json!([])),
    )
    .await?;
    let reconciled = tokio::time::timeout(TIMEOUT, async {
        loop {
            let (_, _, body) = http(
                hub.addr,
                "GET",
                &format!("/v1/instances/{}", stale.as_id().as_str()),
                &[("Cookie", cookie.as_str())],
                None,
            )
            .await?;
            let row: Value = serde_json::from_str(body.trim())?;
            if row["lifecycle"] == json!("exited") {
                return Ok::<_, anyhow::Error>(row);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    assert_eq!(reconciled["lastError"], json!("node-epoch-changed"));
    Ok(())
}

/// A stop for an instance the Node forgot must settle, not hang: the command
/// reaches `settled` and the row reaches `exited` on the Node's "not found".
#[tokio::test]
async fn stop_for_an_instance_the_node_forgot_settles_as_exited() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;

    // The Node answers every `instance.close` with "not found", as a restarted
    // Node does for an instance it never created.
    let node_task = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_millis(300), node.next()).await
            else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("method").and_then(Value::as_str) == Some("instance.close") {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": value.get("id").cloned().unwrap_or(Value::Null),
                    "error": { "code": -32004, "message": "instance not found: ins_gone" }
                });
                let _ = node.send(Message::Text(reply.to_string().into())).await;
                return;
            }
        }
    });

    let (status, _, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{}/commands", instance_id.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        Some(&json!({ "operation": "instance.close", "payload": {} }).to_string()),
    )
    .await?;
    anyhow::ensure!(status == 200, "close {status} {body}");
    let response: Value = serde_json::from_str(body.trim())?;
    assert_eq!(
        response["command"]["state"],
        json!("settled"),
        "a stop the node cannot honour must settle, not hang: {response}"
    );
    node_task.await.ok();

    let (status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{}", instance_id.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "{status} {body}");
    let row: Value = serde_json::from_str(body.trim())?;
    assert_eq!(row["lifecycle"], json!("exited"));
    assert_eq!(row["lastError"], json!("node-lost-instance"));
    Ok(())
}

/// `requested` rows the Node never acknowledged expire to `failed` and stop
/// counting against `maxInstances`, so a wedged host recovers on its own.
#[tokio::test]
async fn unacknowledged_creates_expire_and_release_the_placement_cap() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    // A tiny window so the sweeper fires within the test.
    config.requested_grace_ms = 1;
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let enroll = enroll_token(hub.addr, &cookie).await?;

    // A Node that accepts nothing: every create stays `requested`.
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    let deaf = tokio::spawn(async move {
        while let Ok(Some(Ok(_))) = tokio::time::timeout(Duration::from_secs(6), node.next()).await
        {
        }
    });
    let (status, _, body) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/hosts/{}", host_id.as_id().as_str()),
        &[("Cookie", cookie.as_str())],
        Some(&json!({ "maxInstances": 1 }).to_string()),
    )
    .await?;
    anyhow::ensure!(status == 200, "patch {status} {body}");

    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(
            &json!({
                "kind": "terminal",
                "driver": "shell-pty",
                "hostId": host_id.as_id().as_str()
            })
            .to_string(),
        ),
    )
    .await?;
    anyhow::ensure!(status == 200 || status == 201, "create {status} {body}");
    let created: Value = serde_json::from_str(body.trim())?;
    let first = created["instance"]["instanceId"]
        .as_str()
        .or_else(|| created["instanceId"].as_str())
        .context("created instance id")?
        .to_string();

    // Within a second the sweeper fails it, and a fresh create is accepted
    // again even though the host cap is 1.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let (_, _, body) = http(
                hub.addr,
                "GET",
                &format!("/v1/instances/{first}"),
                &[("Cookie", cookie.as_str())],
                None,
            )
            .await?;
            let row: Value = serde_json::from_str(body.trim())?;
            if row["lifecycle"] == json!("failed") {
                assert_eq!(row["lastError"], json!("create-never-acknowledged"));
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;

    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(
            &json!({
                "kind": "terminal",
                "driver": "shell-pty",
                "hostId": host_id.as_id().as_str()
            })
            .to_string(),
        ),
    )
    .await?;
    anyhow::ensure!(
        status == 200 || status == 201,
        "a host wedged by an expired create must accept a new one: {status} {body}"
    );
    deaf.abort();
    Ok(())
}
