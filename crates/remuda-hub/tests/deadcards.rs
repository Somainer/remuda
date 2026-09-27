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
