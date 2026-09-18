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
    // The cache is stale by definition — the bytes are whatever this Hub last
    // saw before the link went away — so the follower must be told how old
    // they are and why they stopped, or it paints an hour-old frame as live.
    assert_eq!(cached["reason"], json!("node-link-unavailable"));
    let captured_at = cached["capturedAt"]
        .as_str()
        .context("hub-cache snapshot carries no capture time")?;
    assert!(captured_at.ends_with('Z'), "capturedAt {captured_at}");
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

    let hub = spawn(HubConfig::for_test(data_dir.clone())).await?;
    let bootstrap = hub.bootstrap_token.clone();
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

/// Seed an active worker holding `instance_id` on `host_id`.
///
/// The roster row is written straight to the store rather than through
/// `/v1/workers/dispatch`: dispatch would create its *own* instance, and this
/// test needs the worker bound to the instance the reconcile is about.
async fn seed_hub_worker(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    instance_id: &InstanceId,
    name: &str,
) -> Result<String> {
    use remuda_protocol::{EntityMeta, ProjectId, U64, WorkerRoster, WorkerRosterId, WorkerState};
    let store = hub.store().context("store")?;
    let now = remuda_protocol::Timestamp::try_from("2026-01-01T00:00:00.000Z".to_string())?;
    let worker = WorkerRoster {
        meta: EntityMeta {
            id: WorkerRosterId::new(),
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        project_id: ProjectId::new(),
        name: name.to_string(),
        instance_id: Some(instance_id.clone()),
        host_id: host_id.parse()?,
        workspace_id: remuda_protocol::WorkspaceId::new(),
        harness: "claude".into(),
        driver: Some("claude-pty".into()),
        model: None,
        provider_profile_id: None,
        branch: format!("wt/{name}/seed"),
        worktree_path: format!("/tmp/{name}"),
        port_block: None,
        target_dir: None,
        brief_object_id: None,
        task_id: None,
        state: WorkerState::Working,
        watch: None,
        last_nudge_at: None,
        resumed_from: None,
        replace_count: None,
        supply_decision: None,
        reclaimed_bytes: None,
    };
    let row = store.insert_worker(worker, "seed-device".into()).await?;
    Ok(row.meta.id.as_id().to_string())
}

/// Read one worker's row by id.
async fn worker_row(addr: std::net::SocketAddr, cookie: &str, id: &str) -> Result<Value> {
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("/v1/workers/{id}"),
        &[("Cookie", cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "worker {id} {status} {body}");
    Ok(serde_json::from_str(body.trim())?)
}

/// The host's fleet view: capacity, running count and free slots.
async fn hostcap(addr: std::net::SocketAddr, cookie: &str, host_id: &HostId) -> Result<Value> {
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("/v1/hosts/{}/hostcap", host_id.as_id().as_str()),
        &[("Cookie", cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "hostcap {status} {body}");
    Ok(serde_json::from_str(body.trim())?)
}

/// Read one instance row.
async fn instance_row(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &InstanceId,
) -> Result<Value> {
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("/v1/instances/{}", instance_id.as_id().as_str()),
        &[("Cookie", cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "instance {instance_id:?} {status} {body}");
    Ok(serde_json::from_str(body.trim())?)
}

/// The 2026-09-18 demo, end to end: a Node restart loses an instance and
/// everything the Hub derived from it must follow — the row, the worker
/// holding it, and the placement slot it occupied.
///
/// The demo's four zombies stayed `running` after their processes died with
/// the old Node, so they counted against `maxInstances` (placement was
/// unsatisfiable until the cap was raised by hand) while `remuda watch` kept
/// calling them working.
#[tokio::test]
async fn a_lost_instance_fails_its_worker_and_frees_the_slot() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let survivor = InstanceId::new();
    let zombie = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, node_token) =
        node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &survivor, "seed-keep").await?;
    seed_instance(&mut node, &zombie, "seed-lost").await?;
    let worker_id = seed_hub_worker(&hub, host_id.as_id().as_str(), &zombie, "c-demo").await?;
    let survivor_worker =
        seed_hub_worker(&hub, host_id.as_id().as_str(), &survivor, "c-keep").await?;

    // Both rows hold slots, so the cap the restart must free is observable.
    let before = hostcap(hub.addr, &cookie, &host_id).await?;
    let free_before = before["freeSlots"].as_i64().context("freeSlots")?;
    assert_eq!(before["running"], json!(2));
    node.close(None).await?;
    drop(node);

    // The Node comes back under a new epoch. `survivor` is reported live; the
    // zombie is reported *exited* — the Node enumerated the row and confessed
    // it is gone, which must settle rather than shield the Hub's stale copy.
    let _node = node_hello(
        hub.addr,
        &node_token,
        &host_id,
        Some("epoch_two"),
        Some(json!([
            {
                "id": survivor.as_id().as_str(),
                "hostId": host_id.as_id().as_str(),
                "lifecycle": "running"
            },
            {
                "id": zombie.as_id().as_str(),
                "hostId": host_id.as_id().as_str(),
                "lifecycle": "exited"
            }
        ])),
    )
    .await?;

    let zombie_row = instance_row(hub.addr, &cookie, &zombie).await?;
    assert_eq!(zombie_row["lifecycle"], json!("exited"));
    assert_eq!(
        zombie_row["lastError"],
        json!("node-epoch-changed"),
        "the settled row must say why"
    );

    // The worker holding it must stop reading as active: a blocked worker is
    // one its coordinator can act on, a `working` one over a dead process is
    // the lie that kept `remuda watch` reporting progress.
    let zombie_worker = worker_row(hub.addr, &cookie, &worker_id).await?;
    assert_eq!(
        zombie_worker["state"]["state"],
        json!("blocked"),
        "a lost instance must fail the worker holding it: {zombie_worker}"
    );
    assert_eq!(
        zombie_worker["state"]["reason"],
        json!("node-epoch-changed")
    );

    // The survivor and its worker are untouched.
    let survivor_row = instance_row(hub.addr, &cookie, &survivor).await?;
    assert_eq!(survivor_row["lifecycle"], json!("running"));
    let kept_worker = worker_row(hub.addr, &cookie, &survivor_worker).await?;
    assert_eq!(kept_worker["state"]["state"], json!("working"));

    // The slot came back: the demo's sessions were unplaceable until the cap
    // was raised by hand.
    let after = hostcap(hub.addr, &cookie, &host_id).await?;
    assert_eq!(after["running"], json!(1), "only the survivor holds a slot");
    assert_eq!(
        after["freeSlots"].as_i64().context("freeSlots")?,
        free_before + 1,
        "settling the lost row must give its placement slot back"
    );
    Ok(())
}

/// The same reconcile on a host with a neighbour online must not touch the
/// neighbour's rows: the settle is host-scoped, and a restart on one machine
/// is not evidence about another's sessions.
#[tokio::test]
async fn a_node_restart_leaves_another_hosts_rows_alone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_a = HostId::new();
    let host_b = HostId::new();
    let lost = InstanceId::new();
    let neighbour = InstanceId::new();

    let enroll_a = enroll_token(hub.addr, &cookie).await?;
    let (mut node_a, token_a) =
        node_hello(hub.addr, &enroll_a, &host_a, Some("epoch_one"), None).await?;
    let enroll_b = enroll_token(hub.addr, &cookie).await?;
    let (mut node_b, token_b) =
        node_hello(hub.addr, &enroll_b, &host_b, Some("epoch_one"), None).await?;
    seed_instance(&mut node_a, &lost, "seed-a").await?;
    seed_instance(&mut node_b, &neighbour, "seed-b").await?;
    node_a.close(None).await?;
    drop(node_a);

    // Host A restarts and reports nothing at all — every one of its rows is
    // lost. Host B is still online and still holds its own instance.
    let _node_a = node_hello(
        hub.addr,
        &token_a,
        &host_a,
        Some("epoch_two"),
        Some(json!([])),
    )
    .await?;

    let lost_row = instance_row(hub.addr, &cookie, &lost).await?;
    assert_eq!(lost_row["lifecycle"], json!("exited"));
    assert_eq!(lost_row["lastError"], json!("node-epoch-changed"));

    let neighbour_row = instance_row(hub.addr, &cookie, &neighbour).await?;
    assert_eq!(
        neighbour_row["lifecycle"],
        json!("running"),
        "another host's rows are not this restart's to settle: {neighbour_row}"
    );
    // Host B was never touched: its epoch is unchanged, so a reconnect under
    // the same epoch and a matching inventory is a no-op rather than a settle.
    let _node_b = node_hello(
        hub.addr,
        &token_b,
        &host_b,
        Some("epoch_one"),
        Some(json!([{ "id": neighbour.as_id().as_str(), "lifecycle": "running" }])),
    )
    .await?;
    let neighbour_row = instance_row(hub.addr, &cookie, &neighbour).await?;
    assert_eq!(neighbour_row["lifecycle"], json!("running"));
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
    // The fake Node never acknowledges; its RPC must fail fast instead of
    // burning the production 5s accept deadline. With the real deadline the
    // create POST took ~5s, which on a loaded host outran the drain task's
    // fixed socket lifetime and the host went offline before the retry.
    config.command_accept_timeout_ms = 50;
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let enroll = enroll_token(hub.addr, &cookie).await?;

    // A Node that accepts nothing: every create stays `requested`. This task
    // owns the socket for the *entire* test — there is no wall-clock deadline
    // that could close it — drains every frame the Hub forwards, and heartbeats
    // like a real idle Node, so the host cannot lapse offline under load.
    let (node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    let (mut node_tx, mut node_rx) = node.split();
    let deaf = tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(Duration::from_millis(250));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                incoming = node_rx.next() => {
                    if !matches!(incoming, Some(Ok(_))) {
                        break;
                    }
                }
                _ = heartbeat.tick() => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "node.heartbeat",
                        "params": {}
                    });
                    if node_tx
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
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

/// Mint an agent-scoped credential for an instance (Human origin only).
async fn agent_token(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &InstanceId,
) -> Result<String> {
    let (status, _, body) = http(
        addr,
        "POST",
        &format!("/v1/instances/{}/mcp-token", instance_id.as_id().as_str()),
        &[("Cookie", cookie)],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "mcp-token {status} {body}");
    serde_json::from_str::<Value>(body.trim())?["token"]
        .as_str()
        .map(str::to_string)
        .context("agent token")
}

/// Answer the Node side of a delete: accept `instance.purge`, and optionally
/// `instance.close` for the force path. Reports what it saw.
fn spawn_delete_responder(mut node: Ws) -> tokio::task::JoinHandle<(bool, bool)> {
    tokio::spawn(async move {
        let (mut saw_close, mut saw_purge) = (false, false);
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while tokio::time::Instant::now() < deadline && !saw_purge {
            let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_millis(300), node.next()).await
            else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            match value.get("method").and_then(Value::as_str) {
                Some("instance.close") => {
                    saw_close = true;
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "command": { "state": "accepted" } }
                    });
                    let _ = node.send(Message::Text(reply.to_string().into())).await;
                }
                Some("instance.purge") => {
                    saw_purge = true;
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "purged": true, "directoryRemoved": true }
                    });
                    let _ = node.send(Message::Text(reply.to_string().into())).await;
                }
                _ => {}
            }
        }
        (saw_close, saw_purge)
    })
}

/// A stopped session deletes, takes its journal with it, leaves an audit row,
/// and a repeated delete is a 404 rather than an error the UI must special-case.
#[tokio::test]
async fn deleting_a_stopped_instance_removes_it_and_is_idempotent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    // Stop it the ordinary way so the delete takes the non-force path.
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "exit",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": {
                    "kind": "lifecycle",
                    "payload": { "type": "entity", "entityType": "instance", "state": "exited" }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    anyhow::ensure!(recv_json(&mut node).await?.get("result").is_some());

    let responder = spawn_delete_responder(node);
    let id = instance_id.as_id().as_str();
    let (status, _, body) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "delete {status} {body}");
    let deleted: Value = serde_json::from_str(body.trim())?;
    assert_eq!(deleted["deleted"], json!(true));
    assert_eq!(deleted["instanceId"], json!(id));
    assert_eq!(
        deleted["nodePurge"],
        json!("purged"),
        "the node must be asked to purge its own copy: {deleted}"
    );
    let (_, saw_purge) = tokio::time::timeout(TIMEOUT, responder).await??;
    anyhow::ensure!(saw_purge, "hub did not send instance.purge");

    // The row, and its journal, are gone.
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 404, "the instance must be gone");
    let (status, _, journal) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{id}/journal"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    if status == 200 {
        let value: Value = serde_json::from_str(journal.trim())?;
        assert_eq!(
            value["events"].as_array().map(Vec::len).unwrap_or(0),
            0,
            "journal rows must be deleted with the instance: {value}"
        );
    }

    // Idempotent: the second delete is a plain 404.
    let (status, _, _) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 404, "a repeated delete must be idempotent");

    // The audit row outlives the journal it describes.
    let audit = hub
        .store()
        .context("hub store")?
        .audit_for(id.to_string())
        .await?;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["action"], json!("instance.delete"));
    assert_eq!(audit[0]["detail"]["forced"], json!(false));
    anyhow::ensure!(
        audit[0]["deviceId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the audit row must name the device: {audit:?}"
    );
    Ok(())
}

/// A live session is refused without `force=1`, and stopped-then-deleted with it.
#[tokio::test]
async fn deleting_a_live_instance_requires_force_and_settles_it_first() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    let id = instance_id.as_id().as_str();

    let (status, _, body) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(
        status, 409,
        "a running instance must not be deleted: {body}"
    );
    anyhow::ensure!(body.contains("force=1"), "{body}");
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "the refused delete must not remove anything");

    let responder = spawn_delete_responder(node);
    let (status, _, body) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/instances/{id}?force=1"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "force delete {status} {body}");
    let (saw_close, saw_purge) = tokio::time::timeout(TIMEOUT, responder).await??;
    anyhow::ensure!(saw_close, "force must stop the instance before deleting");
    anyhow::ensure!(saw_purge, "force must still purge the node copy");

    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 404);
    let audit = hub
        .store()
        .context("hub store")?
        .audit_for(id.to_string())
        .await?;
    assert_eq!(audit[0]["detail"]["forced"], json!(true), "{audit:?}");
    Ok(())
}

/// Deletion is an operator action: an agent credential cannot call it.
#[tokio::test]
async fn agents_cannot_delete_instances() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    let id = instance_id.as_id().as_str();
    let token = agent_token(hub.addr, &cookie, &instance_id).await?;

    // Even for its own instance, and even with force.
    for path in [
        format!("/v1/instances/{id}"),
        format!("/v1/instances/{id}?force=1"),
    ] {
        let (status, _, body) = http(
            hub.addr,
            "DELETE",
            &path,
            &[("Authorization", format!("Bearer {token}").as_str())],
            None,
        )
        .await?;
        assert_eq!(status, 403, "agents must not delete sessions: {body}");
    }

    // Unauthenticated is refused too, and nothing was removed.
    let (status, _, _) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/instances/{id}"),
        &[],
        None,
    )
    .await?;
    assert_eq!(status, 401);
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(
        status, 200,
        "a refused delete must leave the instance alone"
    );
    Ok(())
}

/// A Node that is offline must not block the delete: the Hub row is what the
/// user asked to remove, and the Node reconciles on reconnect.
#[tokio::test]
async fn deleting_with_the_node_offline_still_removes_the_hub_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let (mut node, _) = node_hello(hub.addr, &enroll, &host_id, Some("epoch_one"), None).await?;
    seed_instance(&mut node, &instance_id, "seed").await?;
    node.close(None).await?;
    drop(node);
    let id = instance_id.as_id().as_str();

    let (status, _, body) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/instances/{id}?force=1"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "delete with node offline {status} {body}");
    let deleted: Value = serde_json::from_str(body.trim())?;
    assert_eq!(deleted["nodePurge"], json!("node-offline"), "{deleted}");

    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 404);
    Ok(())
}
