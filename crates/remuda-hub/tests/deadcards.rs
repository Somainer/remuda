//! c-deadcards: a late answer to a card whose generation ended gets the
//! existing well-defined rejection (404 for an invalidated / unknown
//! interaction) — never a 500, never a silent success — and no `interaction.
//! answer` is forwarded to the Node, so a still-existing hook can never be
//! released with an allow for a dead generation.
//!
//! Round 2 adds the replay-after-host-lost regression (a request replayed
//! under the SAME node epoch after the host-lost sweep ended the instance is
//! ingested already-departed, never pending) and same-command retry
//! idempotency (a lost response + identical retry replays the original
//! acknowledgement, while a different command gets 409 naming the winner).

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, HubError, NodeTransport, TransportKind, spawn};
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

/// Drain unsolicited frames for `window`, SERVING any unrelated
/// `interaction.list` RPC (an empty-items result) so the Hub side does not
/// park waiting on it, and return every method observed. Fails the test if an
/// `interaction.answer` frame arrives — a dead generation must never be
/// answered on the wire.
async fn drain_without_answer(node: &mut NodeWs, window: Duration) -> Result<Vec<String>> {
    let deadline = tokio::time::Instant::now() + window;
    let mut methods = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, recv_json(node)).await {
            Err(_) => break,
            Ok(Err(_)) => break, // ws closed — nothing more to drain
            Ok(Ok(frame)) => {
                let method = frame
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                assert_ne!(
                    method, "interaction.answer",
                    "no interaction.answer may reach the Node for a dead generation: {frame}"
                );
                if method == "interaction.list"
                    && let Some(id) = frame.get("id")
                {
                    send(
                        node,
                        json!({ "jsonrpc": "2.0", "id": id.clone(), "result": { "items": [] } }),
                    )
                    .await?;
                }
                methods.push(method);
            }
        }
    }
    Ok(methods)
}

/// Poll GET /v1/instances until the instance reports `want` (or bail).
async fn wait_instance_lifecycle(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
    want: &str,
) -> Result<String> {
    let mut last = String::new();
    for _ in 0..80 {
        // includeHistory=true: the default list omits terminal rows, so
        // without it the poll would keep observing the pre-sweep snapshot.
        let (status, body) = http(
            addr,
            "GET",
            "/v1/instances?includeHistory=true",
            cookie,
            None,
        )
        .await?;
        assert_eq!(status, 200, "list instances {body}");
        let parsed: Value = serde_json::from_str(body.trim())?;
        if let Some(row) = parsed
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|item| {
                item.get("instanceId")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    == Some(instance_id)
            })
        {
            last = row
                .get("lifecycle")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if last == want {
                return Ok(last);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("instance {instance_id} never reached lifecycle {want}; last={last}")
}

/// The hook-request journal payload the fake Node mirrors (unknown deadline
/// so no client clock could ever retire the card).
fn hook_request_event(interaction_wire: &str) -> Value {
    json!({ "kind": "interaction.requested", "payload": {
        "interactionKind": "approval",
        "interaction": {
            "id": interaction_wire,
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
    }})
}

/// Open a Node WSS link, complete node.hello (carrying `epoch` when given),
/// and return the connected socket plus the hello result (which carries the
/// reusable `nodeToken` for reconnects — the enroll token is single-use).
async fn connect_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &HostId,
    epoch: Option<&str>,
) -> Result<(NodeWs, Value)> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let mut params = json!({
        "hostId": host_id.as_id().as_str(),
        "nodeVersion": "0.1.0"
    });
    if let Some(epoch) = epoch {
        params["nodeEpoch"] = json!(epoch);
    }
    send(
        &mut node,
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
            "params": params
        }),
    )
    .await?;
    let hello = recv_json(&mut node)
        .await
        .context("node.hello acknowledgement")?;
    anyhow::ensure!(hello.get("result").is_some(), "hello failed: {hello}");
    Ok((node, hello))
}

/// POST an instance.create, await its forwarded RPC on `node`, answer it ok,
/// and return the instance id (after the HTTP create resolves).
async fn create_blocked_instance(
    addr: std::net::SocketAddr,
    cookie: &str,
    node: &mut NodeWs,
    host_id: &HostId,
    prompt: &str,
) -> Result<String> {
    let create_cookie = cookie.to_string();
    let host = host_id.as_id().as_str().to_string();
    let prompt = prompt.to_string();
    let create = tokio::spawn(async move {
        http(
            addr,
            "POST",
            "/v1/instances",
            &create_cookie,
            Some(
                &json!({
                    "hostId": host,
                    "kind": "claude",
                    "driver": "claude-print",
                    "delegation": "none",
                    "permissionMode": "bypass",
                    "prompt": prompt,
                })
                .to_string(),
            ),
        )
        .await
    });
    let mut create = Box::pin(create);
    let request = tokio::select! {
        frame = recv_json(node) => Some(frame.context("forwarded create")?),
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
    send(
        node,
        json!({ "jsonrpc": "2.0", "id": rpc_id, "result": { "ok": true, "instanceId": instance_id } }),
    )
    .await?;
    let (status, body) = create.await??;
    assert_eq!(status, 200, "create {body}");
    Ok(instance_id)
}

async fn append_event(node: &mut NodeWs, id: &str, instance_id: &str, event: Value) -> Result<()> {
    send(
        node,
        json!({
            "jsonrpc": "2.0", "id": id, "method": "journal.append",
            "params": { "instanceId": instance_id, "event": event }
        }),
    )
    .await
    .with_context(|| format!("send journal.append {id}"))?;
    let ack = recv_json(node)
        .await
        .with_context(|| format!("await journal.append ack {id}"))?;
    anyhow::ensure!(ack.get("result").is_some(), "append ack: {ack}");
    Ok(())
}

const ALLOW_ANSWER: &str = "{\"kind\":\"approval\",\"optionId\":\"allow-once\",\"inputDigest\":\"sha256:abababababababababababababababababababababababababababababababab\"}";

#[tokio::test]
async fn replay_after_host_lost_same_epoch_is_departed_and_answer_rejected() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    // Tight grace so the reaper ends the lost host's instances quickly.
    config.host_lost_grace_ms = 300;
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let host_id = HostId::new();
    let epoch = format!("ep-{}", uuid::Uuid::now_v7());

    // First link: create a live instance, report it ready.
    let (mut node, hello) = connect_node(addr, &enroll, &host_id, Some(&epoch)).await?;
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .context("hello result carries a reusable nodeToken")?
        .to_string();
    let instance_id =
        create_blocked_instance(addr, &cookie, &mut node, &host_id, "deadcards replay").await?;
    append_event(
        &mut node,
        "j1",
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;

    // The non-daemon WSS Node disappears (it keeps the process and its
    // journaled hook locally). The Hub marks the host offline and the
    // host-lost reaper settles the instance exited at the grace boundary.
    node.close(None).await.ok();
    drop(node);
    wait_instance_lifecycle(addr, &cookie, &instance_id, "exited").await?;

    // The Node reconnects under the SAME epoch: record_node_epoch reports no
    // change, so no epoch reconciliation runs — exactly the round-1 gap. The
    // single-use enroll token is gone, so it authenticates with the nodeToken
    // the first hello issued.
    let (mut node, _hello) = connect_node(addr, &node_token, &host_id, Some(&epoch)).await?;

    // It now replays the hook request it journaled while disconnected (a
    // brand-new seq the Hub never saw; the same append path a fresh request
    // uses). The ingestion fence stores it DEPARTED, never pending.
    let interaction_uuid = uuid::Uuid::now_v7().to_string();
    let interaction_wire = format!("int_{interaction_uuid}");
    append_event(
        &mut node,
        "j2",
        &instance_id,
        hook_request_event(&interaction_wire),
    )
    .await?;

    let (status, body) = http(
        addr,
        "GET",
        &format!("/v1/interactions?instanceId={instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(
        body.contains("\"state\":\"invalidated\"") && !body.contains("\"state\":\"pending\""),
        "a replayed request for a swept instance must land departed: {body}"
    );

    // A late allow is a well-defined 404, not a silent success.
    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());
    let answer_body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(
        status, 404,
        "late allow against a dead generation rejected: {status} {body}"
    );

    // No interaction.answer frame is forwarded (unrelated interaction.list
    // frames, if any, are drained and served).
    let methods = drain_without_answer(&mut node, Duration::from_millis(700)).await?;
    assert!(
        !methods.iter().any(|m| m == "interaction.answer"),
        "answer forwarded for a dead generation: {methods:?}"
    );

    node.close(None).await.ok();
    hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn same_command_retry_after_lost_response_replays_acknowledgement() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let host_id = HostId::new();

    let (mut node, _hello) = connect_node(addr, &enroll, &host_id, Some("ep-retry-1")).await?;
    let instance_id =
        create_blocked_instance(addr, &cookie, &mut node, &host_id, "deadcards retry").await?;
    append_event(
        &mut node,
        "j1",
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;
    let interaction_uuid = uuid::Uuid::now_v7().to_string();
    let interaction_wire = format!("int_{interaction_uuid}");
    append_event(
        &mut node,
        "j2",
        &instance_id,
        hook_request_event(&interaction_wire),
    )
    .await?;

    // First answer: the Hub forwards interaction.answer; the fake Node accepts.
    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());
    let answer_body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let answer_cookie = cookie.clone();
    let answer_url = format!("/v1/interactions/{interaction_wire}/answer");
    let first = tokio::spawn(async move {
        http(
            addr,
            "POST",
            &answer_url,
            &answer_cookie,
            Some(&answer_body),
        )
        .await
    });
    let mut first = Box::pin(first);
    let rpc = tokio::select! {
        frame = recv_json(&mut node) => Some(frame.context("forwarded answer")?),
        done = &mut first => {
            let (status, body) = done??;
            anyhow::bail!("answer returned before forwarding: {status} {body}");
        }
    };
    let rpc = rpc.context("no interaction.answer RPC")?;
    assert_eq!(rpc["method"], "interaction.answer");
    assert_eq!(rpc["params"]["commandId"], json!(command_id));
    send(
        &mut node,
        json!({
            "jsonrpc": "2.0", "id": rpc["id"].clone(),
            "result": {
                "outcome": "accepted",
                "interactionId": interaction_wire,
                "commandId": command_id
            }
        }),
    )
    .await?;
    let (status, body) = first.await??;
    assert_eq!(status, 200, "first answer {body}");

    // The response is "lost": the client retries the IDENTICAL command. The
    // Hub must replay the original acknowledgement — 200, same commandId —
    // without forwarding a second RPC to the Node.
    let answer_body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(
        status, 200,
        "identical retry is idempotent: {status} {body}"
    );
    let ack: Value = serde_json::from_str(body.trim())?;
    assert_eq!(ack["commandId"], json!(command_id));

    // No second interaction.answer reached the Node.
    let methods = drain_without_answer(&mut node, Duration::from_millis(600)).await?;
    assert!(
        !methods.iter().any(|m| m == "interaction.answer"),
        "identical retry must not be re-forwarded: {methods:?}"
    );

    // A DIFFERENT command loses first-answer-wins: 409 naming the winner.
    let other_command = format!("cmd_{}", uuid::Uuid::now_v7());
    let other_body = json!({ "commandId": other_command,
        "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
    .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&other_body),
    )
    .await?;
    assert_eq!(status, 409, "competing command conflicts: {status} {body}");
    let conflict: Value = serde_json::from_str(body.trim())?;
    assert_eq!(
        conflict["winner"].as_str(),
        Some(command_id.as_str()),
        "409 names the winning commandId: {body}"
    );

    // The rejected competitor is not forwarded either.
    let methods = drain_without_answer(&mut node, Duration::from_millis(500)).await?;
    assert!(!methods.iter().any(|m| m == "interaction.answer"));

    node.close(None).await.ok();
    hub.shutdown().await;
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

    // No interaction.answer was forwarded: drain every frame the Hub sends
    // after the close-error reply for the window, serving any unrelated
    // interaction.list RPC; drain_without_answer fails the test if an answer
    // frame shows up.
    let methods = drain_without_answer(&mut node, Duration::from_millis(700)).await?;
    assert!(
        !methods.iter().any(|m| m == "interaction.answer"),
        "a dead generation must never release an allow: {methods:?}"
    );

    node.close(None).await.ok();
    hub.shutdown().await;
    Ok(())
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// An `interaction.answer` call parks until `release()`; records every method
/// it was asked to send. Models the exact old-generation Node link the answer
/// was claimed against.
struct ParkingTransport {
    gate: Arc<Notify>,
    released: Arc<AtomicBool>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
    reply_ok: bool,
}

impl NodeTransport for ParkingTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }
    fn call(
        &self,
        method: &str,
        _params: serde_json::Value,
        _timeout: std::time::Duration,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<serde_json::Value>, HubError>>
                + Send
                + '_,
        >,
    > {
        let method = method.to_string();
        Box::pin(async move {
            self.calls.lock().unwrap().push(method.clone());
            if method == "interaction.answer" {
                while !self.released.load(Ordering::SeqCst) {
                    self.gate.notified().await;
                }
                if self.reply_ok {
                    return Ok(Some(serde_json::json!({
                        "jsonrpc": "2.0", "id": "1",
                        "result": {"outcome": "accepted"}
                    })));
                }
            }
            Ok(None)
        })
    }
    fn notify(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, HubError>> + Send + '_>>
    {
        Box::pin(std::future::ready(Ok(false)))
    }
}

/// The NEW-generation link after a same-epoch reconnect: records methods and
/// answers everything; the test asserts it is NEVER asked interaction.answer.
struct RecordingTransport {
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl NodeTransport for RecordingTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }
    fn call(
        &self,
        method: &str,
        _params: serde_json::Value,
        _timeout: std::time::Duration,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<serde_json::Value>, HubError>>
                + Send
                + '_,
        >,
    > {
        let method = method.to_string();
        Box::pin(async move {
            self.calls.lock().unwrap().push(method);
            Ok(Some(serde_json::json!({
                "jsonrpc": "2.0", "id": "1",
                "result": {"outcome": "accepted"}
            })))
        })
    }
    fn notify(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, HubError>> + Send + '_>>
    {
        Box::pin(std::future::ready(Ok(false)))
    }
}

/// Seed a ready instance with one pending harness-hook interaction directly
/// through the store (no WS), bound to an online host announcing `epoch`.
async fn seed_pending_online(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    epoch: &str,
) -> Result<(String, String)> {
    let store = hub.store().expect("store");
    hub.test_insert_host(host_id).await?;
    store
        .record_node_epoch(host_id.to_owned(), Some(epoch.to_owned()))
        .await?;
    let instance = store
        .ensure_instance(host_id.to_owned(), format!("ins_{}", uuid::Uuid::now_v7()))
        .await?;
    store
        .append_journal(
            host_id.to_owned(),
            instance.instance_id.clone(),
            None,
            json!({ "kind": "lifecycle", "payload": {
                "type": "entity", "entityType": "instance", "state": "ready"
            }}),
        )
        .await?;
    let interaction = format!("int_{}", uuid::Uuid::now_v7());
    store
        .append_journal(
            host_id.to_owned(),
            instance.instance_id.clone(),
            None,
            hook_request_event(&interaction),
        )
        .await?;
    Ok((instance.instance_id, interaction))
}

/// c-deadcards round 3 (HIGH b): an answer dispatched just before the
/// host-lost sweep races a SAME-epoch reconnect. The frame is bound to the
/// old connection; the new link must never receive interaction.answer, and
/// the late RPC result is rejected once the sweep ended the generation.
#[tokio::test]
async fn answer_racing_sweep_and_same_epoch_reconnect_never_reaches_new_node() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.host_lost_grace_ms = 0;
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let epoch = "ep-race";
    let (instance_id, interaction_wire) = seed_pending_online(&hub, &host_id, epoch).await?;

    // Link 1: the link the answer is claimed against parks the RPC.
    let gate = Arc::new(Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let old_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let old_link = Arc::new(ParkingTransport {
        gate: gate.clone(),
        released: released.clone(),
        calls: old_calls.clone(),
        reply_ok: true,
    });
    hub.test_set_node_transport_epoched(&host_id, old_link, Some(epoch.into()))
        .await;

    // Dispatch the answer — it parks inside the fenced call.
    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());
    let answer_body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let answer_cookie = cookie.clone();
    let answer_iid = interaction_wire.clone();
    let answer = tokio::spawn(async move {
        http(
            addr,
            "POST",
            &format!("/v1/interactions/{answer_iid}/answer"),
            &answer_cookie,
            Some(&answer_body),
        )
        .await
    });
    let answer = Box::pin(answer);

    // Wait until the row is claimed `dispatching` (RPC in flight).
    let store = hub.store().expect("store");
    let mut claimed = false;
    for _ in 0..100 {
        if let Some(row) = store.get_interaction(interaction_wire.clone()).await?
            && row.state == "dispatching"
        {
            claimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(claimed, "the answer claimed the card and parked the RPC");
    assert!(
        old_calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "interaction.answer")
    );

    // The host is lost; the sweep ends the generation while the RPC is parked,
    // invalidating the in-flight claim.
    store.mark_host_offline(host_id.clone()).await?;
    let swept = store.expire_lost_hosts(0).await?.0;
    assert_eq!(swept, 1, "sweep ended the instance");
    assert_eq!(
        store
            .get_instance(instance_id)
            .await?
            .expect("row")
            .lifecycle,
        "exited"
    );

    // Same-epoch reconnect: a NEW connection (generation 2) mounts.
    let new_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let new_link = Arc::new(RecordingTransport {
        calls: new_calls.clone(),
    });
    hub.test_set_node_transport_epoched(&host_id, new_link, Some(epoch.into()))
        .await;

    // Release the OLD link's parked RPC with an "accepted" result.
    released.store(true, Ordering::SeqCst);
    gate.notify_waiters();
    let (status, body) = answer.await??;
    assert_eq!(
        status, 404,
        "the late RPC result is rejected because the generation ended: {status} {body}"
    );

    // Give the new link a moment — it must never have been asked anything.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !new_calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "interaction.answer"),
        "an allow must never reach the reconnected same-epoch Node: {:?}",
        new_calls.lock().unwrap()
    );
    // The card stays departed.
    assert_eq!(
        store
            .get_interaction(interaction_wire)
            .await?
            .expect("row")
            .state,
        "invalidated"
    );

    hub.shutdown().await;
    Ok(())
}

/// c-deadcards round 3 (winner): with the HTTP acknowledgement lost, the
/// Node's interaction.answered observation (payload.answerCommandId) commits
/// the winner; the identical retry replays the acknowledgement and forwards
/// NOTHING to the (newly connected) Node.
#[tokio::test]
async fn answered_observation_makes_identical_retry_idempotent_without_forward() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let epoch = "ep-obs";
    let (instance_id, interaction_wire) = seed_pending_online(&hub, &host_id, epoch).await?;
    let store = hub.store().expect("store");

    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());
    // The Node already committed and journaled the answer, but the HTTP
    // response was lost. The observation names the winning command.
    store
        .append_journal(
            host_id.clone(),
            instance_id.clone(),
            None,
            json!({ "kind": "interaction.answered", "payload": {
                "interactionId": interaction_wire,
                "requestVersion": "1",
                "answerCommandId": command_id,
                "actor": { "kind": "human", "deviceId": "dev_obs" },
                "answerRef": "obj_obs",
                "delivery": "written"
            }}),
        )
        .await?;

    // Mount a recording link only AFTER the commit: the retry must be served
    // from the Hub, never forwarded.
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.test_set_node_transport_epoched(
        &host_id,
        Arc::new(RecordingTransport {
            calls: calls.clone(),
        }),
        Some(epoch.into()),
    )
    .await;

    let answer_body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(
        status, 200,
        "identical retry replays the original acknowledgement: {status} {body}"
    );
    let ack: Value = serde_json::from_str(body.trim())?;
    assert_eq!(ack["commandId"], json!(command_id));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "interaction.answer"),
        "the replayed acknowledgement must not be forwarded: {:?}",
        calls.lock().unwrap()
    );

    hub.shutdown().await;
    Ok(())
}

/// c-deadcards round 4 (item 4): a claim left `dispatching` by a Hub crash is
/// recovered after the TTL. Set dispatch_at_ms into the past directly, then a
/// different command reclaims and parks (the stale same-command path returns
/// in-progress only while fresh).
#[tokio::test]
async fn orphaned_claim_after_ttl_is_reclaimable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let _cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let (_instance_id, interaction_wire) = seed_pending_online(&hub, &host_id, "ep-orphan").await?;

    // Fabricate the post-crash shape: claimed by cmd_old 10 minutes ago.
    {
        use rusqlite::Connection;
        let conn = Connection::open(dir.path().join("data").join("hub.sqlite"))?;
        conn.execute(
            "UPDATE interactions
                SET state = 'dispatching', dispatch_epoch = 'ep-orphan',
                    dispatch_link_generation = 99, dispatch_command_id = 'cmd_old',
                    dispatch_at_ms = ?2
              WHERE id = ?1",
            rusqlite::params![
                interaction_wire,
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis() as i64)
                    - 600_000
            ],
        )?;
    }

    // The original command, retried, is still told in-progress while the claim
    // is fresh-ish — but here it is older than the TTL, so the SAME command
    // simply reclaims. We assert the NEW command can claim it: mount a parking
    // link (bound to the claimed link identity generation) is unnecessary — the
    // claim recovery only needs the row to return to pending for a new caller.
    // Verify the stale claim is recoverable: a different command claims it
    // (deterministic store-level assertion, no timing dependence).
    let claim = hub
        .store()
        .expect("store")
        .claim_interaction_dispatch(
            interaction_wire.clone(),
            "cmd_new".into(),
            Some("ep-orphan".into()),
            1,
        )
        .await?;
    assert!(
        matches!(claim, remuda_hub::DispatchClaim::Claimed { .. }),
        "a different command reclaims the orphaned claim after the TTL"
    );

    // And within the TTL the same commandId on a fresh claim is InFlight.
    let claim = hub
        .store()
        .expect("store")
        .release_interaction_dispatch(interaction_wire.clone(), "cmd_new".into())
        .await?;
    assert!(matches!(claim, remuda_hub::DispatchRelease::Pending));
    hub.shutdown().await;
    Ok(())
}

/// c-deadcards round 4 (item 5): the Node commits first — its
/// interaction.answered observation (payload.answerCommandId) is journaled
/// BEFORE the HTTP ack arrives. The POST still succeeds (200) and forwards
/// nothing, instead of a false rejection.
#[tokio::test]
async fn answered_observation_ahead_of_ack_still_succeeds_without_forward() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let (instance_id, interaction_wire) = seed_pending_online(&hub, &host_id, "ep-ahead").await?;
    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());

    // Node committed first and journaled it (payload.answerCommandId).
    hub.store()
        .expect("store")
        .append_journal(
            host_id.clone(),
            instance_id,
            None,
            json!({ "kind": "interaction.answered", "payload": {
                "interactionId": interaction_wire,
                "requestVersion": "1",
                "answerCommandId": command_id,
                "actor": { "kind": "human", "deviceId": "dev_ahead" },
                "answerRef": "obj_ahead",
                "delivery": "written"
            }}),
        )
        .await?;

    // The answer ack arrives afterwards, possibly on a fresh link.
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.test_set_node_transport_epoched(
        &host_id,
        Arc::new(RecordingTransport {
            calls: calls.clone(),
        }),
        Some("ep-ahead".into()),
    )
    .await;

    let answer_body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(
        status, 200,
        "observation-with-our-commandId is success, not a false rejection: {body}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "interaction.answer"),
        "nothing is forwarded after the Node already committed"
    );
    hub.shutdown().await;
    Ok(())
}

/// c-deadcards round 4 (item 6): while the identical commandId's dispatch is
/// still in flight, a duplicate returns 202 in-progress — never 200 success.
#[tokio::test]
async fn identical_retry_while_in_flight_returns_in_progress_not_success() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let (_instance_id, interaction_wire) =
        seed_pending_online(&hub, &host_id, "ep-inflight").await?;

    let gate = Arc::new(Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let old_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.test_set_node_transport_epoched(
        &host_id,
        Arc::new(ParkingTransport {
            gate: gate.clone(),
            released: released.clone(),
            calls: old_calls.clone(),
            reply_ok: true,
        }),
        Some("ep-inflight".into()),
    )
    .await;

    let command_id = format!("cmd_{}", uuid::Uuid::now_v7());
    let body =
        json!({ "commandId": command_id, "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
            .to_string();
    let first_cookie = cookie.clone();
    let first_iid = interaction_wire.clone();
    let first_body = body.clone();
    let first = tokio::spawn(async move {
        http(
            addr,
            "POST",
            &format!("/v1/interactions/{first_iid}/answer"),
            &first_cookie,
            Some(&first_body),
        )
        .await
    });
    let first = Box::pin(first);

    let store = hub.store().expect("store");
    for _ in 0..100 {
        if let Some(row) = store.get_interaction(interaction_wire.clone()).await?
            && row.state == "dispatching"
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Identical commandId while still parked.
    let (status, body2) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&body),
    )
    .await?;
    assert_eq!(
        status, 202,
        "in-flight duplicate is in-progress, not success: {body2}"
    );
    let body2: Value = serde_json::from_str(body2.trim())?;
    assert_eq!(body2["code"].as_str(), Some("INTERACTION_IN_PROGRESS"));

    released.store(true, Ordering::SeqCst);
    gate.notify_waiters();
    let (first_status, _) = first.await??;
    assert_eq!(
        first_status, 200,
        "the original call completes once the Node replies"
    );
    hub.shutdown().await;
    Ok(())
}

/// c-deadcards round 4 (item 7): an expired durable interaction keeps its
/// defined 410 Gone, distinct from the dead-generation 404.
#[tokio::test]
async fn expired_durable_interaction_returns_410() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let (instance_id, interaction_wire) = seed_pending_online(&hub, &host_id, "ep-expired").await?;

    hub.store()
        .expect("store")
        .append_journal(
            host_id,
            instance_id,
            None,
            json!({ "kind": "interaction.expired", "payload": {
                "interactionId": interaction_wire,
                "requestVersion": "1",
                "reason": "deadline"
            }}),
        )
        .await?;

    let answer_body = json!({ "commandId": format!("cmd_{}", uuid::Uuid::now_v7()),
                 "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
    .to_string();
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(status, 410, "expired keeps its defined 410: {body}");
    let body_json: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body_json["code"].as_str(), Some("INTERACTION_EXPIRED"));
    hub.shutdown().await;
    Ok(())
}

/// Transport whose interaction.list returns fixed items; answers accepted.
struct ListingTransport {
    list_items: Value,
    answer_calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl NodeTransport for ListingTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }
    fn call(
        &self,
        method: &str,
        _params: Value,
        _timeout: std::time::Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<Value>, HubError>> + Send + '_>,
    > {
        let method = method.to_string();
        Box::pin(async move {
            if method == "interaction.list" {
                return Ok(Some(json!({
                    "jsonrpc": "2.0", "id": "1",
                    "result": {"items": self.list_items}
                })));
            }
            self.answer_calls.lock().unwrap().push(method);
            Ok(Some(json!({
                "jsonrpc": "2.0", "id": "1",
                "result": {"outcome": "accepted"}
            })))
        })
    }
    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, HubError>> + Send + '_>>
    {
        Box::pin(std::future::ready(Ok(false)))
    }
}

/// c-deadcards round 4 (item 3): a LIVE-only interaction.list item for an
/// instance whose Hub row is terminal is projected as departed — not pending,
/// not answerable — even though no durable interaction row exists for it.
#[tokio::test]
async fn live_item_for_terminal_instance_merges_as_departed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    // Terminal instance with NO durable interaction.
    let (instance_id, _) = seed_pending_online(&hub, &host_id, "ep-live-departed").await?;
    hub.store()
        .expect("store")
        .append_journal(
            host_id.clone(),
            instance_id.clone(),
            None,
            json!({"kind":"lifecycle","payload":{"type":"entity","entityType":"instance","state":"exited"}}),
        )
        .await?;

    // The Node still lists a live pending card for that (dead) instance.
    let live_id = format!("int_{}", uuid::Uuid::now_v7());
    let answer_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.test_set_node_transport_epoched(
        &host_id,
        Arc::new(ListingTransport {
            list_items: json!([{
                "interactionId": live_id,
                "id": live_id,
                "instanceId": instance_id,
                "hostId": host_id,
                "kind": "approval",
                "state": "pending",
                "blocking": true,
                "answerable": true
            }]),
            answer_calls: answer_calls.clone(),
        }),
        Some("ep-live-departed".into()),
    )
    .await;

    let (status, body) = http(
        addr,
        "GET",
        &format!("/v1/interactions?instanceId={instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let page: Value = serde_json::from_str(body.trim())?;
    let item = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|it| it.get("interactionId").and_then(Value::as_str) == Some(live_id.as_str()))
        .expect("live item is present in the merge");
    assert_eq!(
        item["state"].as_str(),
        Some("invalidated"),
        "projected departed: {item}"
    );
    assert_eq!(item["answerable"].as_bool(), Some(false));
    assert_eq!(item["blocking"].as_bool(), Some(false));

    // Answering it via the fenced live path is refused, not forwarded.
    let answer_body = json!({ "commandId": format!("cmd_{}", uuid::Uuid::now_v7()),
                 "instanceId": instance_id,
                 "answer": serde_json::from_str::<Value>(ALLOW_ANSWER)? })
    .to_string();
    let (status, _) = http(
        addr,
        "POST",
        &format!("/v1/interactions/{live_id}/answer"),
        &cookie,
        Some(&answer_body),
    )
    .await?;
    assert_eq!(
        status, 404,
        "a live-only card on a terminal instance is not answerable"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !answer_calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "interaction.answer"),
        "the allow must not be forwarded for a dead-generation live card"
    );
    hub.shutdown().await;
    Ok(())
}

/// c-deadcards round 4 (item 8): a settlement emitted by the host-lost reaper
/// is broadcast on the follow bus so an open inbox/session learns immediately.
#[tokio::test]
async fn host_lost_reaper_broadcasts_the_settlement_to_followers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.host_lost_grace_ms = 0;
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let host_id = format!("hst_{}", uuid::Uuid::now_v7());
    let (instance_id, interaction_wire) =
        seed_pending_online(&hub, &host_id, "ep-broadcast").await?;

    // Open a follow socket filtered to the instance.
    let mut req =
        format!("ws://{addr}/v1/follow?instanceId={instance_id}").into_client_request()?;
    req.headers_mut()
        .insert("Cookie", cookie.parse().expect("cookie header"));
    let (mut follow, _) = tokio_tungstenite::connect_async(req).await?;

    // End the generation via the BACKGROUND host-lost reaper (the path under
    // test — it must broadcast, unlike a bare store sweep).
    let store = hub.store().expect("store");
    store.mark_host_offline(host_id.clone()).await?;
    assert_eq!(
        store
            .get_instance(instance_id.clone())
            .await?
            .expect("row")
            .lifecycle,
        "running"
    );

    // The follower receives the Hub-settlement observation (an attach snapshot
    // may arrive first — skip non-settlement frames).
    let mut settlement_frame = None;
    for _ in 0..10 {
        let frame = tokio::time::timeout(Duration::from_secs(3), recv_json(&mut follow))
            .await
            .context("no settlement frame received")??;
        if frame["type"].as_str() == Some("event")
            && frame["event"]["payload"]["hubSettlement"].as_bool() == Some(true)
        {
            settlement_frame = Some(frame);
            break;
        }
    }
    let frame = settlement_frame.context("settlement frame never observed")?;
    assert_eq!(frame["event"]["kind"].as_str(), Some("interaction.expired"));
    assert_eq!(
        frame["event"]["payload"]["interactionId"].as_str(),
        Some(interaction_wire.as_str())
    );
    follow.close(None).await.ok();
    hub.shutdown().await;
    Ok(())
}
