//! c-deadcards: a late answer to a card whose generation ended gets the
//! existing well-defined rejection (404 for an invalidated / unknown
//! interaction) — never a 500, never a silent success — and no `interaction.
//! answer` is forwarded to the Node, so a still-existing hook can never be
//! released with an allow for a dead generation.

use anyhow::{Context, Result};
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

type NodeWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&str>,
) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\n"
    );
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
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
    Ok((status, rest.to_string()))
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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let mut stream = TcpStream::connect(addr).await?;
    let body = format!("{{\"bootstrapToken\":\"{bootstrap}\",\"deviceName\":\"deadcards-test\"}}");
    let req = format!(
        "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let head = text.split_once("\r\n\r\n").map(|(h, _)| h).unwrap_or(&text);
    cookie_from(head).context("login cookie")
}

async fn enroll_token(addr: std::net::SocketAddr, cookie: &str) -> Result<String> {
    let (status, rest) = http(addr, "POST", "/v1/hosts/enroll-token", cookie, Some("{}")).await?;
    anyhow::ensure!(status == 200, "enroll-token {status} {rest}");
    let value: Value = serde_json::from_str(rest.trim())?;
    value["token"]
        .as_str()
        .map(str::to_string)
        .context("enroll token")
}

async fn recv_json(node: &mut NodeWs) -> Result<Value> {
    let msg = tokio::time::timeout(TIMEOUT, node.next())
        .await
        .context("ws recv timeout")?
        .context("ws closed")??;
    match msg {
        Message::Text(text) => Ok(serde_json::from_str(&text)?),
        other => anyhow::bail!("unexpected ws message: {other:?}"),
    }
}

async fn send(node: &mut NodeWs, frame: Value) -> Result<()> {
    node.send(Message::Text(frame.to_string().into())).await?;
    Ok(())
}

#[tokio::test]
async fn late_answer_after_instance_end_is_rejected_and_never_forwarded() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;

    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    send(
        &mut node,
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        }),
    )
    .await?;
    recv_json(&mut node).await?;

    // Create an instance; answer the forwarded create and journal a live,
    // blocked approval (durable pending row + unknown deadline).
    let create_cookie = cookie.clone();
    let create = tokio::spawn(async move {
        http(
            addr,
            "POST",
            "/v1/instances",
            &create_cookie,
            Some(
                &json!({
                    "hostId": host_id.as_id().as_str(),
                    "kind": "claude",
                    "driver": "claude-print",
                    "delegation": "none",
                    "permissionMode": "bypass",
                    "prompt": "deadcards late answer",
                })
                .to_string(),
            ),
        )
        .await
    });
    let mut create = Box::pin(create);
    let request = tokio::select! {
        frame = recv_json(&mut node) => Some(frame.context("forwarded create")?),
        done = &mut create => {
            let (status, body) = done??;
            anyhow::bail!("create returned before forwarding an RPC: {status} {body}");
        }
    };
    let request = request.context("no create request")?;
    assert_eq!(request["method"], "instance.create");
    let rpc_id = request["id"].clone();
    let instance_id = request["params"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    // The interaction id must be a concrete canonical UUIDv7 (the answer
    // path TryFroms the URL id); the protocol new() wrapper keeps the id
    // opaque, so mint the UUIDv7 directly. The durable/journal entity uses
    // the wire spelling `int_<uuid>`; the answer URL uses the bare typed id.
    let interaction_uuid = uuid::Uuid::now_v7().to_string();
    let interaction_wire = format!("int_{interaction_uuid}");
    send(
        &mut node,
        json!({ "jsonrpc": "2.0", "id": rpc_id, "result": { "ok": true, "instanceId": instance_id } }),
    )
    .await?;

    async fn append(node: &mut NodeWs, id: &str, instance_id: &str, event: Value) -> Result<()> {
        send(
            node,
            json!({
                "jsonrpc": "2.0", "id": id, "method": "journal.append",
                "params": { "instanceId": instance_id, "event": event }
            }),
        )
        .await?;
        let ack = recv_json(node).await?;
        anyhow::ensure!(ack.get("result").is_some(), "append ack: {ack}");
        Ok(())
    }

    append(
        &mut node,
        "j1",
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;
    append(
        &mut node,
        "j2",
        &instance_id,
        json!({ "kind": "interaction.requested", "payload": {
            "interactionKind": "approval",
            "interaction": {
                "id": &interaction_wire,
                "kind": "approval",
                "state": "pending",
                "blocking": true,
                "answerable": true,
                "carrier": "harness-hook",
                "deadline": { "state": "unknown" },
                "resolution": { "state": "unknown" },
                "request": {
                    "kind": "approval",
                    "title": "Bash",
                    "description": "rm -rf /tmp/deadcards",
                    "options": [
                        { "id": "allow-once", "label": "允许一次", "effect": "allow-once" },
                        { "id": "deny", "label": "拒绝", "effect": "deny" }
                    ],
                    "requestedPermissionsRef": null,
                    "inputDigest": "sha256:abababababababababababababababababababababababababababababababab"
                }
            }
        }}),
    )
    .await?;
    let (status, body) = create.await??;
    assert_eq!(status, 200, "create {body}");

    // Stop the instance; the Node reports it does not know the instance
    // (-32004). The Hub settles the row exited (node-lost-instance) and, in
    // the same change, invalidates the pending interaction.
    let stop_cookie = cookie.clone();
    let iid2 = instance_id.clone();
    let stop = tokio::spawn(async move {
        http(
            addr,
            "POST",
            &format!("/v1/instances/{iid2}/commands"),
            &stop_cookie,
            Some(&json!({ "operation": "instance.close", "payload": {} }).to_string()),
        )
        .await
    });
    let close_rpc = recv_json(&mut node).await.context("forwarded close")?;
    assert_eq!(close_rpc["method"], "instance.close");
    send(
        &mut node,
        json!({
            "jsonrpc": "2.0", "id": close_rpc["id"].clone(),
            "error": { "code": -32004, "message": "unknown instance" }
        }),
    )
    .await?;
    let (status, _) = stop.await??;
    assert_eq!(status, 200, "the stop settles instead of failing");

    // The durable card is now invalidated (generation-ended), not pending.
    let mut observed = String::new();
    for _ in 0..40 {
        let (status, body) = http(
            addr,
            "GET",
            &format!("/v1/interactions?instanceId={}", instance_id),
            &cookie,
            None,
        )
        .await?;
        assert_eq!(status, 200);
        observed = body;
        if observed.contains("\"state\":\"invalidated\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        observed.contains("\"state\":\"invalidated\""),
        "card must be invalidated after the instance end: {observed}"
    );

    // The late answer: a well-defined 404 (the entity no longer exists), not
    // a 500 and not a silent 200.
    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());
    let answer_body = json!({
        "commandId": command_id,
        "answer": { "kind": "approval", "optionId": "allow-once", "inputDigest": "sha256:abababababababababababababababababababababababababababababababab" }
    })
    .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{}/answer", interaction_wire),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(
        status, 404,
        "late answer rejected as not-found: {status} {body}"
    );

    // No interaction.answer was forwarded: the Node receives nothing after
    // the close-error reply.
    let leaked = tokio::time::timeout(Duration::from_millis(500), recv_json(&mut node)).await;
    match leaked {
        Err(_elapsed) => {} // no frame — correct
        Ok(Ok(frame)) => {
            let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
            assert_ne!(
                method, "interaction.answer",
                "a dead generation must never release an allow: {frame}"
            );
        }
        Ok(Err(_)) => {} // ws closed — also fine
    }

    node.close(None).await.ok();
    hub.shutdown().await;
    Ok(())
}
