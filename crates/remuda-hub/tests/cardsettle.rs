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
/// The Hub's settlement broadcast ring capacity (settlement_bus).
const SETTLEMENT_RING: usize = 64;
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
    /// Live items the fake serves from `interaction.list`.
    live_items: Arc<Mutex<Vec<Value>>>,
    /// Set if the socket dropped: a disconnect is a test failure.
    disconnected: Arc<AtomicBool>,
    /// Reconnect credential minted by the first `node.hello`.
    node_token: Arc<Mutex<Option<String>>>,
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
        let live_items: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let node_token: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (mut sink, mut stream) = ws.split();
        let calls_task = calls.clone();
        let live_items_task = live_items.clone();
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
                            // The merge's live fan-out: serve the scripted live
                            // items (filtered to the queried instance when one
                            // is given).
                            "interaction.list" => {
                                let queried_instance =
                                    frame.pointer("/params/instanceId").and_then(Value::as_str);
                                let items: Vec<Value> = live_items_task
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|item| {
                                        queried_instance.is_none_or(|id| {
                                            item.get("instanceId").and_then(Value::as_str)
                                                == Some(id)
                                        })
                                    })
                                    .cloned()
                                    .collect();
                                json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "result": { "items": items, "nextCursor": null }
                                })
                            }
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
            live_items,
            disconnected,
            node_token,
            _task: task,
        };
        node.send(json!({
            "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "nodeEpoch": "cs-fake-epoch-1"
            }
        }))?;
        let hello = node.await_result("hello").await?;
        if let Some(token) = hello["result"]["nodeToken"].as_str() {
            *node.node_token.lock().unwrap() = Some(token.to_string());
        }
        Ok((node, host_id))
    }

    /// The reconnect credential the first hello minted.
    fn node_token(&self) -> String {
        self.node_token
            .lock()
            .unwrap()
            .clone()
            .expect("hello result carried a nodeToken")
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

    /// Script the live `interaction.list` page the fake Node serves.
    fn set_live_items(&self, items: Vec<Value>) {
        *self.live_items.lock().unwrap() = items;
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

    // r3 item 2: the Node that failed to purge still lists the id as pending.
    // The tombstone must suppress that live copy in the merge, so the deleted
    // card can never come back into the inbox or the badge.
    node.set_live_items(vec![json!({
        "interactionId": interaction_wire,
        "id": interaction_wire,
        "instanceId": instance_id,
        "hostId": host_id.as_id().as_str(),
        "kind": "approval",
        "state": "pending",
        "blocking": true,
        "answerable": true
    })]);
    let (status, body) = http(addr, "GET", "/v1/interactions", &cookie, None).await?;
    assert_eq!(status, 200);
    let page: Value = serde_json::from_str(&body)?;
    let ids: Vec<&str> = page["items"]
        .as_array()
        .context("items array")?
        .iter()
        .filter_map(|item| item.get("interactionId").and_then(Value::as_str))
        .collect();
    assert!(
        !ids.contains(&interaction_wire.as_str()),
        "a tombstoned id's stale Node-pending copy must not return after delete: {body}"
    );

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

/// r2 item 3: an aged (outside the 24 h display retention) invalidated durable
/// row must still suppress a restarted Node's `interaction.list` copy of the
/// same id, which the stale process serves as pending. Display retention and
/// authoritative dedup are separate.
#[tokio::test]
async fn aged_terminal_row_suppresses_the_nodes_pending_copy_from_the_merge() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    let (instance_id, interaction_wire) =
        seed_live_card(addr, &cookie, &node, &host_id, "cardsettle dedup").await?;

    // End the generation on the Hub (node-lost stop) → durable invalidated.
    let (status, _) = http(
        addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        &cookie,
        Some(&json!({ "operation": "instance.close", "payload": {} }).to_string()),
    )
    .await?;
    assert_eq!(status, 200);
    let mut state = String::new();
    for _ in 0..40 {
        state = poll_card_state(addr, &cookie, &instance_id).await?;
        if state == "invalidated" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(state, "invalidated");

    // Age the durable row past the 24 h departed retention.
    if let Some(store) = hub.store() {
        store
            .test_backdate_interaction(
                interaction_wire.clone(),
                "1999-01-01T00:00:00.000Z".to_string(),
            )
            .await?;
    }

    // The restarted process wrongly still serves the same id as pending.
    node.set_live_items(vec![json!({
        "interactionId": interaction_wire,
        "id": interaction_wire,
        "instanceId": instance_id,
        "hostId": host_id.as_id().as_str(),
        "kind": "approval",
        "state": "pending",
        "blocking": true,
        "answerable": true
    })]);

    // The merge must not bring the stale pending copy back.
    let (status, body) = http(addr, "GET", "/v1/interactions", &cookie, None).await?;
    assert_eq!(status, 200);
    let page: Value = serde_json::from_str(&body)?;
    let ids: Vec<&str> = page["items"]
        .as_array()
        .context("items array")?
        .iter()
        .filter_map(|item| item.get("interactionId").and_then(Value::as_str))
        .collect();
    assert!(
        !ids.contains(&interaction_wire.as_str()),
        "the aged terminal row's stale Node copy must not re-queue: {body}"
    );

    hub.shutdown().await;
    Ok(())
}

/// r7 item 1, regression (b): the REAL hello reconcile-then-replay sequence.
/// A Node loses the Hub, journals an approval and then the exit; on reconnect
/// the hello reconcile marks the instance exited BEFORE the journal catch-up
/// replays the request. The late request must never become a pending/blocked
/// card — it lands invalidated with a settlement notice on the follow bus.
#[tokio::test]
async fn hello_reconcile_exits_then_replayed_request_never_reopens_the_card() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    // A running instance on the Node (created over RPC, ready frame appended).
    let create_body = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "permissionMode": "bypass",
        "prompt": "cardsettle r7 reconnect reconcile",
    })
    .to_string();
    let (status, create_response) =
        http(addr, "POST", "/v1/instances", &cookie, Some(&create_body)).await?;
    assert_eq!(status, 200, "create {create_response}");
    let body: Value = serde_json::from_str(&create_response)?;
    let instance_id = body["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    node.append(
        "jready",
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;

    // An unfiltered follower is the settlement socket; settle it BEFORE the
    // reconnect so the late card's notice must arrive live (not from the
    // connect replay).
    let mut follow_req = format!("ws://{addr}/v1/follow").into_client_request()?;
    follow_req.headers_mut().insert("Cookie", cookie.parse()?);
    let (mut follow, _) = tokio_tungstenite::connect_async(follow_req).await?;

    // RECONNECT on a fresh socket (a second hello over the same connection is
    // refused): a NEW node epoch and an empty, attested inventory — the Node no
    // longer holds the instance, so the reconcile marks it exited.
    let mut reconnect_req = format!("ws://{addr}/v1/node").into_client_request()?;
    reconnect_req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", node.node_token()).parse()?,
    );
    let (mut reconnect, _) = tokio_tungstenite::connect_async(reconnect_req).await?;
    reconnect
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "hello2", "method": "node.hello",
                "params": {
                    "hostId": host_id.as_id().as_str(),
                    "nodeVersion": "0.1.0",
                    "nodeEpoch": "cs-r7-epoch-2",
                    "instanceStoreFound": true,
                    "instances": []
                }
            })
            .to_string()
            .into(),
        ))
        .await?;

    /// Read frames on a raw Node socket until the result with `want_id`.
    async fn read_result<S>(socket: &mut S, want_id: &str) -> Result<Value>
    where
        S: futures::Stream<
                Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
            > + Unpin,
    {
        loop {
            let opt = tokio::time::timeout(TIMEOUT, socket.next())
                .await
                .with_context(|| format!("timeout waiting for {want_id}"))?
                .context("node socket closed")?;
            let Ok(Message::Text(text)) = opt else {
                continue;
            };
            let frame: Value = serde_json::from_str(&text)?;
            if frame.get("id").and_then(Value::as_str) == Some(want_id) {
                anyhow::ensure!(frame.get("result").is_some(), "unexpected frame: {frame}");
                return Ok(frame);
            }
        }
    }

    read_result(&mut reconnect, "hello2").await?;

    // The owner is exited before any late frame arrives.
    let (status, instance_response) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200, "{instance_response}");
    let instance: Value = serde_json::from_str(&instance_response)?;
    assert_eq!(instance["lifecycle"], "exited", "reconcile ended the owner");
    let activity_after_reconcile = instance["activity"].as_str().unwrap_or("").to_string();
    assert_ne!(activity_after_reconcile, "blocked");

    // The journal catch-up now replays the approval the Node journaled while
    // the Hub link was down — over the reconnected link.
    let late_id = format!("int_{}", uuid::Uuid::now_v7());
    reconnect
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "jlate", "method": "journal.append",
                "params": {
                    "instanceId": instance_id,
                    "event": approval_requested_event(&late_id),
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    read_result(&mut reconnect, "jlate").await?;

    // r8 item 5: after the second hello, nodes.insert routes instance RPCs to
    // THIS reconnect socket, not the FakeNode's original one. Hand it a
    // servicing watcher for the rest of the test that acks every request and
    // records inbound methods, so the post-answer no-forward assertion below
    // watches the socket the Hub would actually send interaction.answer on.
    let reconnect_methods: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let reconnect_watch = {
        let methods = Arc::clone(&reconnect_methods);
        tokio::spawn(async move {
            use futures::{SinkExt, StreamExt};
            let (mut sink, mut stream) = reconnect.split();
            while let Some(Ok(Message::Text(text))) =
                tokio::time::timeout(Duration::from_secs(10), stream.next())
                    .await
                    .ok()
                    .flatten()
            {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(id) = frame.get("id").cloned() else {
                    continue;
                };
                if let Some(method) = frame.get("method").and_then(Value::as_str) {
                    methods.lock().unwrap().push(method.to_string());
                }
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
                if sink
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    // The settlement notice for the late card rides the dedicated settlement
    // bus — skip unrelated follow frames (journal events, host updates).
    let mut got_notice = false;
    while let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(3), follow.next()).await
    {
        let Message::Text(text) = msg else { continue };
        let frame: Value = serde_json::from_str(&text)?;
        if frame["type"] == "settlement" && frame["interactionId"] == json!(late_id) {
            assert_eq!(frame["state"], "invalidated");
            assert_eq!(frame["reason"], "generation-ended");
            got_notice = true;
            break;
        }
    }
    assert!(
        got_notice,
        "the replayed request produced a live settlement notice"
    );
    drop(follow);

    // Durable truth: the card is invalidated, never pending/actionable.
    let (status, body) = http(
        addr,
        "GET",
        &format!("/v1/interactions?instanceId={instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let page: Value = serde_json::from_str(&body)?;
    let late = page["items"]
        .as_array()
        .context("items")?
        .iter()
        .find(|item| item["interactionId"] == json!(late_id))
        .context("late interaction row")?;
    assert_eq!(late["state"], "invalidated", "{body}");
    assert_eq!(late["blocking"], json!(false), "the card is not blocking");

    // The owner was never flipped back to blocked.
    let (status, instance_response) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200, "{instance_response}");
    let instance: Value = serde_json::from_str(&instance_response)?;
    assert_eq!(instance["lifecycle"], "exited");
    assert_ne!(
        instance["activity"].as_str().unwrap_or(""),
        "blocked",
        "a replayed request never re-blocks an ended instance"
    );

    // r9 item 4(b): POSITIVELY prove the watcher is the socket nodes.insert
    // routes host RPCs to. A GET /v1/interactions fans `interaction.list`
    // out to every connected owner node, so require THIS watcher to service
    // one before the answer window. Without that proof a watcher installed on
    // a socket that got swapped before the window would record nothing at all
    // and make the "no interaction.answer" assertion below vacuous.
    let proof_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen_list = reconnect_methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "interaction.list");
        if seen_list {
            break;
        }
        assert!(
            tokio::time::Instant::now() < proof_deadline,
            "the reconnect watcher never serviced interaction.list — it is not the live-routed socket"
        );
        // The watcher replies {ok:true}; the merge treats that as no items.
        let _ = http(addr, "GET", "/v1/interactions", &cookie, None).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // An answer now gets the existing not-pending rejection and never reaches
    // the Node. r8 item 5 / r9 item 4(b): against the RECONNECT socket,
    // proven above to be the one the Hub routes to after the second hello.
    let (status, answer_body) = post_answer(addr, &cookie, &late_id).await?;
    assert_eq!(status, 404, "late answer rejected: {answer_body}");
    tokio::time::sleep(NO_FORWARD_WINDOW).await;
    node.assert_no_answer_forwarded()?;
    let answers_on_reconnect: usize = reconnect_methods
        .lock()
        .unwrap()
        .iter()
        .filter(|method| method.as_str() == "interaction.answer")
        .count();
    assert_eq!(
        answers_on_reconnect, 0,
        "a 404 late answer must never be forwarded to the reconnected Node"
    );
    reconnect_watch.abort();

    hub.shutdown().await;
    Ok(())
}

/// r8 item 2 (OA6): host loss is CONTACT loss, not process end. The
/// host-lost sweep marks a still-running instance's ROW exited/host-lost but
/// must NOT settle its cards and must NOT stamp process-end evidence
/// (`ended_at`). On a SAME-epoch reconnect whose inventory lists the instance
/// live, the Hub revives the row BEFORE the journal catch-up; an approval the
/// still-running child then raises lands PENDING and is answerable (the
/// answer is forwarded over the RECONNECT socket — item 5).
#[tokio::test]
async fn host_loss_sweep_keeps_cards_and_revives_a_live_inventory_row_on_reconnect() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    // A live instance with one pending, blocking card.
    let (instance_id, first_card) =
        seed_live_card(addr, &cookie, &node, &host_id, "cardsettle r8 host loss").await?;
    assert_eq!(
        poll_card_state(addr, &cookie, &instance_id).await?,
        "pending"
    );

    // Contact loss past grace. The sweep marks the ROW host-lost; its
    // settlement must be EMPTY (the card is not invalidated).
    let store = hub.store().expect("hub store exposed to tests");
    store
        .mark_host_offline(host_id.as_id().as_str().to_string())
        .await?;
    let (swept, settlement) = store.expire_lost_hosts(0).await?;
    assert_eq!(swept, 1, "the live instance row is marked host-lost");
    assert!(
        settlement.interactions.is_empty(),
        "a host-lost sweep must not settle cards: {settlement:?}"
    );

    // Durable truth after the sweep: row exited/host-lost with NO ended_at,
    // card still pending and still blocking.
    let (status, body) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let swept_row: Value = serde_json::from_str(&body)?;
    assert_eq!(swept_row["lifecycle"], "exited");
    assert_eq!(swept_row["lastError"], json!("host-contact-lost"));
    assert!(
        swept_row.get("endedAt").is_none() || swept_row["endedAt"].is_null(),
        "contact loss must not stamp process-end evidence: {swept_row}"
    );
    assert_eq!(
        poll_card_state(addr, &cookie, &instance_id).await?,
        "pending"
    );

    // SAME-epoch reconnect (the same Node process) with the instance listed
    // LIVE. A raw socket services journal.append RPCs and captures the answer
    // frame the Hub must later forward to THIS socket.
    let mut reconnect_req = format!("ws://{addr}/v1/node").into_client_request()?;
    reconnect_req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", node.node_token()).parse()?,
    );
    let (mut reconnect, _) = tokio_tungstenite::connect_async(reconnect_req).await?;
    reconnect
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "hello2", "method": "node.hello",
                "params": {
                    "hostId": host_id.as_id().as_str(),
                    "nodeVersion": "0.1.0",
                    "nodeEpoch": "cs-fake-epoch-1",
                    "instanceStoreFound": true,
                    "instances": [
                        { "id": instance_id, "hostId": host_id.as_id().as_str(),
                          "lifecycle": "running", "activity": "idle" }
                    ]
                }
            })
            .to_string()
            .into(),
        ))
        .await?;

    // Service the raw reconnect socket: ack every JSON-RPC request, recording
    // a forwarded interaction.answer; result frames (hello2) are ignored.
    let answers = Arc::new(Mutex::new(Vec::<Value>::new()));
    let answers_task = Arc::clone(&answers);
    let service_task = tokio::spawn(async move {
        use futures::{SinkExt, StreamExt};
        let (mut sink, mut stream) = reconnect.split();
        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(Duration::from_secs(8), stream.next()).await
        {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
            if method == "interaction.answer" {
                answers_task.lock().unwrap().push(frame.clone());
            }
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
            if sink
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Read the hello2 result (serviced by the first socket's task only when
    // the Hub routes by generation; the frame actually returns on whichever
    // socket owns the request) — give the Hub a moment to process revival,
    // then assert the row came back.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (status, body) = http(
            addr,
            "GET",
            &format!("/v1/instances/{instance_id}"),
            &cookie,
            None,
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        let row: Value = serde_json::from_str(&body)?;
        if row["lifecycle"] == "running" {
            assert!(
                row.get("lastError").is_none() || row["lastError"].is_null(),
                "revival clears the host-lost marker: {row}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "row never revived: {row}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The still-running child raises a NEW approval over the reconnected
    // link. The host-lost row was NOT a dead owner, so this must land PENDING
    // — not invalidated at insert.
    let new_card = format!("int_{}", uuid::Uuid::now_v7());
    node.append("jnew", &instance_id, approval_requested_event(&new_card))
        .await?;
    // jnew goes through the FIRST fake-node socket (which still services
    // frames); the Hub accepts it regardless of which socket carries it.
    let (status, interactions_body) = http(
        addr,
        "GET",
        &format!("/v1/interactions?instanceId={instance_id}"),
        &cookie,
        None,
    )
    .await?;
    assert_eq!(status, 200, "{interactions_body}");
    let page: Value = serde_json::from_str(&interactions_body)?;
    for wire in [&first_card, &new_card] {
        let item = page["items"]
            .as_array()
            .context("items")?
            .iter()
            .find(|item| {
                item.pointer("/event/payload/interaction/id")
                    .and_then(Value::as_str)
                    == Some(wire.as_str())
                    || item["interactionId"] == json!(wire)
            })
            .unwrap_or_else(|| panic!("card {wire} missing: {page}"));
        assert_eq!(item["state"], json!("pending"), "{wire} must stay pending");
        assert_eq!(item["blocking"], json!(true), "{wire} must stay blocking");
    }

    // The new approval is ANSWERABLE: the HTTP answer succeeds and the Hub
    // forwards interaction.answer over the RECONNECT socket (the owner row is
    // live again, so no pre-RPC 404).
    let (status, answer_body) = post_answer(addr, &cookie, &new_card).await?;
    assert_eq!(
        status, 200,
        "the revived owner's card is answerable: {answer_body}"
    );
    let forwarded_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !answers.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < forwarded_deadline,
            "interaction.answer never forwarded over the reconnect socket"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let forwarded = answers.lock().unwrap()[0].clone();
    assert_eq!(forwarded["method"], json!("interaction.answer"));
    assert_eq!(
        forwarded
            .pointer("/params/interactionId")
            .or_else(|| forwarded.pointer("/params/interaction/id"))
            .and_then(Value::as_str),
        Some(new_card.as_str())
    );

    service_task.abort();
    hub.shutdown().await;
    Ok(())
}

/// r8 item 3: a follower that subscribes with a PRE-POPULATED settlement
/// history (hundreds of invalidated rows for the life of the database) must
/// not replay that history when its ring lags. Its lag cursor is seeded at
/// the durable max at subscribe time; only settlements committed AFTER
/// subscribe (the blocked sweep) are recovered before the gap.
#[tokio::test]
async fn follower_lag_never_replays_pre_subscribe_history() -> Result<()> {
    let mut config = HubConfig::for_test(tempfile::tempdir()?.path().join("data"));
    config.follow_buffer_events = 1;
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    // Create an instance carrying many pending cards, then sweep them away with
    // an epoch change BEFORE the follower connects — this is the pre-existing
    // history a fresh inbox follower's initial list already reflects.
    let (old_instance, old_cards) =
        ready_instance_with_cards(&hub, &cookie, &node, &host_id, "r8-old", 700).await?;
    let old_generation =
        sweep_with_new_epoch(&addr, &node, &host_id, "cs-r8item3-old", json!([]), 1).await?;
    assert!(
        old_cards.len() > SETTLEMENT_RING,
        "the pre-populated history must exceed the settlement ring"
    );
    // The old instance row is exited after the pre-subscribe sweep.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = http(
            addr,
            "GET",
            &format!("/v1/instances/{old_instance}"),
            &cookie,
            None,
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        let row: Value = serde_json::from_str(&body)?;
        if row["lifecycle"] == "exited" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "old row never exited: {row}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The bounded connect replay shows at most the 5 NEWEST old rows.
    let snapshot_newest_old: std::collections::HashSet<String> =
        old_cards.iter().rev().take(5).cloned().collect();

    // Backdate the pre-populated settlements OUTSIDE the 5-minute connect
    // replay window, so the only way they could reach this follower is the
    // (cursed) None-cursor history walk. The seeded cursor must exclude them.
    hub.test_backdate_interactions(old_cards.clone(), "2000-01-01T00:00:00.000Z")
        .await?;

    // A NEW instance with its own pending cards, the post-subscribe sweep.
    let (new_instance, new_cards) =
        ready_instance_with_cards(&hub, &cookie, &node, &host_id, "r8-new", 70).await?;

    // Connect the follower with a tiny receive window and never read until the
    // blocked state settles (same parking technique as the r7 lag-order test).
    let mut follow_req = format!("ws://{addr}/v1/follow").into_client_request()?;
    follow_req.headers_mut().insert("Cookie", cookie.parse()?);
    let follow_tcp = tokio::net::TcpStream::connect(addr).await?;
    #[cfg(unix)]
    {
        nix::sys::socket::setsockopt(&follow_tcp, nix::sys::socket::sockopt::RcvBuf, &256)?;
    }
    let (mut follow, _) = tokio_tungstenite::client_async(follow_req, follow_tcp).await?;
    node.append(
        "jpad-r8",
        &new_instance,
        json!({ "kind": "message", "payload": { "text": "x".repeat(16 * 1024) } }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The post-subscribe sweep: new epoch, empty attested inventory.
    let new_generation =
        sweep_with_new_epoch(&addr, &node, &host_id, "cs-r8item3-new", json!([]), 2).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Unblock and read until the settlement-backpressure gap.
    let mut got: Vec<String> = Vec::new();
    let mut old_seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    'read: while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(next) = tokio::time::timeout(remaining, follow.next()).await.ok() else {
            break 'read;
        };
        let Some(Ok(Message::Text(text))) = next else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text)?;
        match frame.get("type").and_then(Value::as_str) {
            Some("settlement") => {
                let id = frame["interactionId"]
                    .as_str()
                    .context("settlement interactionId")?
                    .to_string();
                if old_cards.contains(&id) {
                    old_seen.push(id.clone());
                }
                got.push(id);
            }
            Some("gap") if frame["reason"] == "settlement-backpressure" => break 'read,
            _ => {}
        }
    }

    assert!(
        old_seen.is_empty() || old_seen.iter().all(|id| snapshot_newest_old.contains(id)),
        "only the connect replay's few newest-old rows may arrive, never a lag drain of history: \
         {old_seen:?}"
    );
    assert!(
        old_seen.len() <= 5,
        "the windowed connect replay is bounded to the newest few: {old_seen:?}"
    );
    assert_eq!(
        got.len() - old_seen.len(),
        new_cards.len(),
        "exactly the post-subscribe sweep drains before the gap"
    );
    let mut want = new_cards.clone();
    want.sort();
    let mut sorted_got = got.clone();
    sorted_got.sort();
    assert_eq!(
        sorted_got, want,
        "the recovered ids are exactly the new sweep"
    );

    old_generation.abort();
    new_generation.abort();
    hub.shutdown().await;
    Ok(())
}

/// Create an instance, drive it ready, and append `card_count` pending
/// approval cards over the fake Node socket. Returns `(instance_id, card_ids)`.
async fn ready_instance_with_cards(
    hub: &remuda_hub::RunningHub,
    cookie: &str,
    node: &FakeNode,
    host_id: &HostId,
    label: &str,
    card_count: usize,
) -> Result<(String, Vec<String>)> {
    let addr = hub.addr;
    let create_body = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "permissionMode": "bypass",
        "prompt": format!("cardsettle {label} lag seed"),
    })
    .to_string();
    let (status, create_response) =
        http(addr, "POST", "/v1/instances", cookie, Some(&create_body)).await?;
    assert_eq!(status, 200, "create {create_response}");
    let create_json: Value = serde_json::from_str(&create_response)?;
    let instance_id = create_json["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    node.append(
        &format!("jready-{label}"),
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;
    let mut card_ids = Vec::with_capacity(card_count);
    for n in 0..card_count {
        let id = format!("int_{label}_{n:05}");
        node.append(
            &format!("jreq-{label}-{n}"),
            &instance_id,
            approval_requested_event(&id),
        )
        .await?;
        card_ids.push(id);
    }
    Ok((instance_id, card_ids))
}

/// Open a new node socket and send a same-host hello carrying a NEW node epoch
/// (the reconcile sweeps the instances the inventory omits), reading the
/// hello result. The returned task services the new generation socket (the
/// Hub routes RPCs there after the hello) for the test's lifetime; abort it
/// at teardown.
async fn sweep_with_new_epoch(
    addr: &std::net::SocketAddr,
    node: &FakeNode,
    host_id: &HostId,
    epoch: &str,
    instances: Value,
    hello_seq: u8,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", node.node_token()).parse()?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(req).await?;
    let hello_id = format!("hello-sweep-{hello_seq}");
    let (mut sink, mut stream) = socket.split();
    sink.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": hello_id, "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "nodeEpoch": epoch,
                "instanceStoreFound": true,
                "instances": instances
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    // Read until the hello result, acking any interleaved RPC.
    let mut got_result = false;
    while let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(8), stream.next()).await
    {
        let Message::Text(text) = msg else { continue };
        let frame: Value = serde_json::from_str(&text)?;
        let Some(id) = frame.get("id").cloned() else {
            continue;
        };
        if id == hello_id {
            anyhow::ensure!(frame.get("result").is_some(), "sweep hello failed: {frame}");
            got_result = true;
            break;
        }
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
        sink.send(Message::Text(reply.to_string().into())).await?;
    }
    anyhow::ensure!(got_result, "never got {hello_id} result");
    // Keep the generation alive and service every further RPC until teardown —
    // dropping this socket marks the host offline and blocks later creates.
    let task = tokio::spawn(async move {
        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(Duration::from_secs(60), stream.next()).await
        {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
            if sink
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(task)
}

/// r7 item 3 (the r6 item 2 test as actually asked): a REAL follower through
/// `/v1/follow`, with its writer blocked by socket backpressure, receives a
/// ONE-timestamp settlement sweep LARGER than the 64-notice broadcast ring
/// (the node-epoch reconcile settles many cards on one instance in one
/// transaction). When the client drains, EVERY dropped settlement must be
/// recovered from the durable lag drain in ascending `(updated_at, id)` order
/// BEFORE the single `settlement-backpressure` gap — a high id published first
/// would advance the cursor past same-batch low ids and skip them forever.
#[tokio::test]
async fn follower_lag_drains_a_large_single_sweep_before_the_gap() -> Result<()> {
    let mut config = HubConfig::for_test(tempfile::tempdir()?.path().join("data"));
    // A 1-deep pump→writer queue so a backed-up socket parks the pump fast;
    // the settlement ring itself stays the production 64.
    config.follow_buffer_events = 1;
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let (node, host_id) = FakeNode::spawn(addr, &enroll).await?;

    // One instance carrying MANY pending cards.
    let create_body = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "permissionMode": "bypass",
        "prompt": "cardsettle r7 lag order",
    })
    .to_string();
    let (status, create_response) =
        http(addr, "POST", "/v1/instances", &cookie, Some(&create_body)).await?;
    assert_eq!(status, 200, "create {create_response}");
    let create_json: Value = serde_json::from_str(&create_response)?;
    let instance_id = create_json["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    node.append(
        "jready",
        &instance_id,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": "ready"
        }}),
    )
    .await?;

    const CARD_COUNT: usize = 70; // > the 64-notice settlement ring
    let mut card_ids = Vec::with_capacity(CARD_COUNT);
    for n in 0..CARD_COUNT {
        // Zero-padded so lexicographic id order is the numeric cursor order.
        let id = format!("int_r7lag_{n:05}");
        node.append(
            &format!("jreq-{n}"),
            &instance_id,
            approval_requested_event(&id),
        )
        .await?;
        card_ids.push(id);
    }

    // A REAL unfiltered follower over a socket whose receive buffer is tiny,
    // and we never read from it: the writer blocks on the kernel buffer, the
    // 1-deep queue fills, and the follower task parks WITHOUT polling the
    // settlement ring.
    let mut follow_req = format!("ws://{addr}/v1/follow").into_client_request()?;
    follow_req.headers_mut().insert("Cookie", cookie.parse()?);
    let follow_tcp = tokio::net::TcpStream::connect(addr).await?;
    // Shrink the kernel receive window before the WS handshake: SO_RCVBUF via
    // nix (Linux doubles/clamps it to the 4 KiB floor). This bounds how much
    // the unread client can buffer, so the server writer provably blocks on a
    // few large frames.
    #[cfg(unix)]
    {
        nix::sys::socket::setsockopt(&follow_tcp, nix::sys::socket::sockopt::RcvBuf, &256)?;
    }
    let (mut follow, _) = tokio_tungstenite::client_async(follow_req, follow_tcp).await?;

    // ONE large JOURNAL event (the follower receives every one unfiltered),
    // bigger than the shrunken receive window: the writer blocks finishing
    // this single frame and can never service the settlement ring while the
    // client never reads. No flood — one blocked send is the same parking
    // condition, and keeps the settlement drain behind only ~16 KiB.
    node.append(
        "jpad-0",
        &instance_id,
        json!({ "kind": "message", "payload": { "text": "x".repeat(16 * 1024) } }),
    )
    .await?;
    // Let the blocked state settle: the writer is parked inside sink.send and
    // the pump cannot flush a second frame.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The one-timestamp sweep: a new epoch with an empty, attested inventory
    // reconciles the instance exited and invalidates all 70 cards in ONE
    // transaction at one updated_at, broadcast in the select's id order.
    let mut reconnect_req = format!("ws://{addr}/v1/node").into_client_request()?;
    reconnect_req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", node.node_token()).parse()?,
    );
    let (mut reconnect, _) = tokio_tungstenite::connect_async(reconnect_req).await?;
    reconnect
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "hello2", "method": "node.hello",
                "params": {
                    "hostId": host_id.as_id().as_str(),
                    "nodeVersion": "0.1.0",
                    "nodeEpoch": "cs-r7lag-epoch-2",
                    "instanceStoreFound": true,
                    "instances": []
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    // Drain the hub→node frames on the raw reconnect socket up to hello2.
    while let Ok(Some(Ok(msg))) =
        tokio::time::timeout(Duration::from_secs(8), reconnect.next()).await
    {
        let Message::Text(text) = msg else { continue };
        let frame: Value = serde_json::from_str(&text)?;
        if frame.get("id").and_then(Value::as_str) == Some("hello2") {
            anyhow::ensure!(
                frame.get("result").is_some(),
                "reconcile hello failed: {frame}"
            );
            break;
        }
    }
    // Give the blocked follower's ring time to hold/skip the whole burst.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // UNBLOCK: pump every queued server frame out of the client socket. The
    // pump resumes, observes the broadcast Lagged, and drains the durable
    // settlement pages before its gap notice.
    let mut got: Vec<String> = Vec::new();
    let mut settlement_gaps = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    'read: while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(next) = tokio::time::timeout(remaining, follow.next()).await.ok() else {
            break 'read;
        };
        let Some(Ok(Message::Text(text))) = next else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text)?;
        match frame.get("type").and_then(Value::as_str) {
            Some("settlement") => got.push(
                frame["interactionId"]
                    .as_str()
                    .context("settlement interactionId")?
                    .to_string(),
            ),
            // The settlement lag drain's own trailing frame.
            Some("gap") if frame["reason"] == "settlement-backpressure" => {
                settlement_gaps += 1;
                break 'read;
            }
            // Journal frames and the journal-bus backpressure gap are noise
            // for this assertion.
            _ => {}
        }
    }

    // All 70 recovered, exactly once, in ascending cursor order, and all
    // before the gap.
    assert_eq!(
        got.len(),
        CARD_COUNT,
        "every settlement of the over-ring sweep is recovered before the gap"
    );
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted, "drained settlements arrive in cursor order");
    assert_eq!(got, card_ids, "the recovered ids are exactly the sweep");
    assert_eq!(
        settlement_gaps, 1,
        "one settlement gap follows the full drain"
    );

    hub.shutdown().await;
    Ok(())
}
