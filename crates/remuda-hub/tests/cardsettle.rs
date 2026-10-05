//! c-cardsettle: when an instance ends, its still-pending card is invalidated
//! in the same transaction.
//!
//!  * A late answer gets the existing state-derived rejection — 404 for an
//!    invalidated / deleted interaction, 410 for expired — never a 500, never a
//!    silent success.
//!  * No `interaction.answer` is ever forwarded to the Node, so a still-
//!    existing hook can never be released with an allow for a dead/deleted
//!    generation — including when the instance rows were deleted after a
//!    rejected purge (the tombstone case, r2 item 2).

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);
/// Window over which EVERY Node frame is inspected after the late answer.
const NO_FORWARD_WINDOW: Duration = Duration::from_millis(800);

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
    let body = format!("{{\"bootstrapToken\":\"{bootstrap}\",\"deviceName\":\"cardsettle-test\"}}");
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

/// A fake Node whose socket is owned by one servicing task for the whole test,
/// so EVERY inbound frame is serviced and recorded — the answer path's
/// interaction.list fan-out gets a real reply instead of queueing and being
/// mistaken for (or hiding) a later frame (r2 item 5).
struct FakeNode {
    /// Scripted Node→Hub frames.
    outbound_tx: UnboundedSender<String>,
    /// Hub→Node result frames (no `method`), by id.
    result_rx: Arc<tokio::sync::Mutex<UnboundedReceiver<Value>>>,
    /// Every inbound RPC METHOD observed, in arrival order.
    calls: Arc<Mutex<Vec<String>>>,
    /// Set if the socket dropped: a disconnect is a test failure.
    disconnected: Arc<AtomicBool>,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    async fn spawn(addr: std::net::SocketAddr, enroll: &str) -> Result<(Self, HostId)> {
        let host_id = HostId::new();
        let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        let (outbound_tx, mut outbound_rx) = unbounded_channel::<String>();
        let (result_tx, result_rx) = unbounded_channel::<Value>();
        let result_rx = Arc::new(tokio::sync::Mutex::new(result_rx));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let (mut sink, mut stream) = ws.split();
        let calls_task = calls.clone();
        let disconnected_task = disconnected.clone();
        let disconnected_out = disconnected.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    incoming = stream.next() => {
                        let Some(Ok(msg)) = incoming else {
                            disconnected_task.store(true, Ordering::SeqCst);
                            break;
                        };
                        let Message::Text(text) = msg else { continue };
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                        let Some(method) = frame.get("method").and_then(Value::as_str) else {
                            // A result for one of our scripted RPCs.
                            let _ = result_tx.send(frame);
                            continue;
                        };
                        calls_task.lock().unwrap().push(method.to_string());
                        let Some(id) = frame.get("id").cloned() else { continue };
                        let reply = match method {
                            // Accept the create; the Hub-generated id rides in
                            // the params.
                            "instance.create" | "instance.resume" => json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {
                                    "ok": true,
                                    "instanceId": frame["params"]["instanceId"].clone()
                                }
                            }),
                            // The stop/purge paths must not block the Hub;
                            // "unknown instance" is what a restarted process
                            // answers (and item 2 rejects the purge).
                            "instance.close" | "instance.cancel" | "instance.purge" => json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32004, "message": "unknown instance" }
                            }),
                            // The merge's live fan-out: an empty live page.
                            "interaction.list" => json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "items": [], "nextCursor": null }
                            }),
                            // A violation: never accept an answer for a dead
                            // generation. Recorded as the method regardless;
                            // reply an error so the client can't misread it.
                            "interaction.answer" => json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32004, "message": "unknown instance" }
                            }),
                            other => json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32601, "message": format!("unhandled fake method {other}") }
                            }),
                        };
                        if sink.send(Message::Text(reply.to_string().into())).await.is_err() {
                            disconnected_task.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    outgoing = outbound_rx.recv() => {
                        let Some(text) = outgoing else { break };
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            disconnected_out.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        });
        let node = FakeNode {
            outbound_tx,
            result_rx,
            calls,
            disconnected,
            _task: task,
        };
        node.send(json!({
            "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        }))?;
        node.await_result("hello").await?;
        Ok((node, host_id))
    }

    fn send(&self, frame: Value) -> Result<()> {
        self.outbound_tx
            .send(frame.to_string())
            .map_err(|_| anyhow::anyhow!("fake node task stopped"))
    }

    async fn await_result(&self, want_id: &str) -> Result<Value> {
        let mut rx = self.result_rx.lock().await;
        let frame = tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Some(frame) = rx.recv().await
                    && frame.get("id").and_then(Value::as_str) == Some(want_id)
                {
                    return frame;
                }
            }
        })
        .await
        .with_context(|| format!("timeout waiting for result {want_id}"))?;
        anyhow::ensure!(frame.get("result").is_some(), "unexpected frame: {frame}");
        Ok(frame)
    }

    async fn append(&self, id: &str, instance_id: &str, event: Value) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "journal.append",
            "params": { "instanceId": instance_id, "event": event }
        }))?;
        self.await_result(id).await?;
        Ok(())
    }

    /// After the late-answer window: no `interaction.answer` was serviced and
    /// the socket never dropped (r2 item 5).
    fn assert_no_answer_forwarded(&self) -> Result<()> {
        assert!(
            !self.disconnected.load(Ordering::SeqCst),
            "node socket disconnected during the no-forward window"
        );
        let calls = self.calls.lock().unwrap();
        let offenders: Vec<&String> = calls
            .iter()
            .filter(|method| method.as_str() == "interaction.answer")
            .collect();
        assert!(
            offenders.is_empty(),
            "a dead/deleted generation must never release an allow; frames={calls:?}"
        );
        Ok(())
    }

    fn observed_methods(&self) -> HashSet<String> {
        self.calls.lock().unwrap().iter().cloned().collect()
    }
}

fn approval_requested_event(interaction_wire: &str) -> Value {
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
                "description": "rm -rf /tmp/cardsettle",
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

/// Drive an instance with one unknown-deadline pending approval up to the
/// point the card is durable-pending. Returns the ids; the caller ends the
/// generation.
async fn seed_live_card(
    addr: std::net::SocketAddr,
    cookie: &str,
    node: &FakeNode,
    host_id: &HostId,
    prompt: &str,
) -> Result<(String, String)> {
    let create_cookie = cookie.to_string();
    let request_body = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "permissionMode": "bypass",
        "prompt": prompt,
    })
    .to_string();
    let create = tokio::spawn(async move {
        http(
            addr,
            "POST",
            "/v1/instances",
            &create_cookie,
            Some(&request_body),
        )
        .await
    });
    // The fake loop already answered instance.create when the HTTP call
    // returns; the journal frames follow.
    let (status, body) = create.await??;
    assert_eq!(status, 200, "create {body}");
    let instance_id = serde_json::from_str::<Value>(&body)?["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    let interaction_wire = format!("int_{}", uuid::Uuid::now_v7());
    node.append(
        "j1",
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;
    node.append(
        "j2",
        &instance_id,
        approval_requested_event(&interaction_wire),
    )
    .await?;
    Ok((instance_id, interaction_wire))
}

async fn poll_card_state(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
) -> Result<String> {
    let (status, body) = http(
        addr,
        "GET",
        &format!("/v1/interactions?instanceId={instance_id}"),
        cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let page: Value = serde_json::from_str(&body)?;
    Ok(page["items"][0]["state"]
        .as_str()
        .unwrap_or("missing")
        .to_string())
}

async fn post_answer(
    addr: std::net::SocketAddr,
    cookie: &str,
    interaction_wire: &str,
) -> Result<(u16, String)> {
    let answer_body = json!({
        "commandId": format!("cmd_{}", uuid::Uuid::now_v7()),
        "answer": {
            "kind": "approval",
            "optionId": "allow-once",
            "inputDigest": "sha256:abababababababababababababababababababababababababababababababab"
        }
    })
    .to_string();
    http(
        addr,
        "POST",
        &format!("/v1/interactions/{interaction_wire}/answer"),
        cookie,
        Some(&answer_body),
    )
    .await
}

/// The node-unknown stop path settles the instance and invalidates the card;
/// a late answer is 404 and no interaction.answer is ever forwarded.
#[tokio::test]
async fn late_answer_after_instance_end_is_rejected_and_never_forwarded() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    let (instance_id, interaction_wire) =
        seed_live_card(addr, &cookie, &node, &host_id, "cardsettle stop").await?;

    // The Node no longer knows the instance: the stop settles it exited and
    // invalidates the card.
    let (status, _) = http(
        addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        &cookie,
        Some(&json!({ "operation": "instance.close", "payload": {} }).to_string()),
    )
    .await?;
    assert_eq!(status, 200, "the stop settles instead of failing");

    let mut state = String::new();
    for _ in 0..40 {
        state = poll_card_state(addr, &cookie, &instance_id).await?;
        if state == "invalidated" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        state, "invalidated",
        "card must be invalidated after the end"
    );

    // The late answer: 404, not 500 / 200.
    let (status, body) = post_answer(addr, &cookie, &interaction_wire).await?;
    assert_eq!(status, 404, "late answer rejected as not-found: {body}");

    // Give every possible delayed fan-out a full window to show up; the
    // servicing loop answered the queued interaction.list, so nothing is
    // hidden behind it.
    tokio::time::sleep(NO_FORWARD_WINDOW).await;
    node.assert_no_answer_forwarded()?;

    hub.shutdown().await;
    Ok(())
}

/// r2 item 2: force-delete whose Node purge is rejected removes the durable
/// rows, but a late answer must still get the existing rejection (404) via the
/// tombstone — and must never fan interaction.answer out to the still-
/// connected Node.
#[tokio::test]
async fn late_answer_after_delete_with_rejected_purge_is_rejected_and_not_forwarded() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    let (instance_id, interaction_wire) =
        seed_live_card(addr, &cookie, &node, &host_id, "cardsettle delete").await?;

    // Force delete: the fake Node rejects instance.close AND instance.purge
    // (-32004 unknown instance); the Hub deletes the record anyway.
    let (status, body) = http(
        addr,
        "DELETE",
        &format!("/v1/instances/{instance_id}?force=1"),
        &cookie,
        None,
    )
    .await?;
    assert!(
        status == 200 || status == 204,
        "delete proceeds despite the rejected purge: {status} {body}"
    );

    // Both the close and the rejected purge were actually attempted, so this
    // really exercised the "purge failed, rows deleted" path.
    let methods = node.observed_methods();
    assert!(
        methods.contains("instance.purge"),
        "the node must have been asked to purge: {methods:?}"
    );

    // The durable card row is gone with the instance…
    let state = poll_card_state(addr, &cookie, &instance_id).await?;
    assert_eq!(state, "missing", "the deleted instance's card row is gone");

    // …yet the late answer is still 404 via the retained tombstone.
    let (status, body) = post_answer(addr, &cookie, &interaction_wire).await?;
    assert_eq!(
        status, 404,
        "late answer after delete rejected from the tombstone: {body}"
    );

    tokio::time::sleep(NO_FORWARD_WINDOW).await;
    node.assert_no_answer_forwarded()?;

    hub.shutdown().await;
    Ok(())
}
