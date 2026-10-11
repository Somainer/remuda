//! c-acceptrace round 2 item 1: the e2e fake Node's `instance.send` reply must
//! carry the SAME acceptance envelope the real DevNode serializes, and the Hub
//! must mark the command accepted from that reply.
//!
//! This drives the ACTUAL Hub HTTP send path against a fake Node connected
//! over the real WebSocket. The fake Node awaits the durable ack of a
//! `journal.append` user message (exactly as the e2e harness does) and then
//! answers with a chosen reply shape.
//!
//! * command-envelope reply (what `DevNode::enqueue_existing` +
//!   `accept_command` produces): the command is `accepted`, not
//!   queued/reconciling.
//! * legacy bare `{"ok":true}` reply: the command is NOT accepted — it rests
//!   for journal convergence, which is the flake this guard exists to catch.

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use remuda_hub::HubConfig;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

/// Which `instance.send` reply the fake Node returns.
#[derive(Clone, Copy)]
enum ReplyShape {
    /// `{accepted:true, command:{state:"accepted", commandId}}` — the real
    /// DevNode envelope.
    CommandEnvelope,
    /// `{accepted:true}` at the result top level (also accepted by the Hub but
    /// not what send produces).
    TopLevelAccepted,
    /// The pre-fix fake-node reply.
    BareOk,
}

impl ReplyShape {
    fn result(self, command_id: &str) -> Value {
        match self {
            ReplyShape::CommandEnvelope => json!({
                "ok": true,
                "accepted": true,
                "command": { "state": "accepted", "commandId": command_id },
            }),
            ReplyShape::TopLevelAccepted => json!({ "accepted": true }),
            ReplyShape::BareOk => json!({ "ok": true }),
        }
    }
}

/// Connect a fake Node that enrolls, announces one workspace/host, and answers:
/// - `instance.create` with an accepted envelope + instanceId;
/// - `instance.send` by FIRST durably appending the user message over
///   journal.append (awaiting the ack) and THEN replying with `shape`.
async fn spawn_fake_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace_id: &str,
    shape: ReplyShape,
) -> anyhow::Result<()> {
    let enroll = hub.mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {enroll}").parse()?,
    );
    let (mut ws, _) = connect_async(req).await?;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-acceptrace",
                "label": "acceptrace-node",
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "workspaces": [{
                        "workspaceId": workspace_id, "hostId": host_id,
                        "root": "/tmp/remuda-acceptrace",
                    }],
                    "workspaceRevision": 1,
                    "cli": [{"kind":"claude","version":"0.0.0",
                              "absolutePath":"/usr/bin/claude","authState":"logged_in"}],
                },
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut ws).await?;
    assert!(hello["result"].is_object(), "hello rejected: {hello}");

    // Drive the WS loop until the connection is dropped by the test end.
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(id) = frame.get("id").cloned() else { continue };
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
            let result = match method {
                "instance.create" => {
                    let iid = params["instanceId"].as_str().unwrap_or("").to_owned();
                    json!({ "accepted": true, "instanceId": iid })
                }
                "instance.send" => {
                    // Mirror the real e2e fake Node / DevNode contract:
                    // journal the user message durably and wait for the append
                    // ack BEFORE replying to the send.
                    let iid = params["instanceId"].as_str().map(str::to_string).unwrap_or_default();
                    let command_id = params["commandId"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_default();
                    let node = command_id
                        .strip_prefix("cmd_")
                        .map_or_else(|| format!("obj_{command_id}"), |_| format!("obj_{command_id}"));
                    ws.send(Message::Text(
                        json!({
                            "jsonrpc": "2.0", "id": "jappend", "method": "journal.append",
                            "params": {
                                "instanceId": iid,
                                "event": {
                                    "kind": "message",
                                    "completeness": "structured",
                                    "payload": {
                                        "role": "user", "origin": "human",
                                        "text": "acceptrace prompt", "nodeId": node,
                                        "messageId": node, "commandId": command_id,
                                        "revision": "1", "operation": "open",
                                        "phase": "input", "status": "complete",
                                    },
                                },
                            },
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .ok();
                    // Read and discard the append ack; if it errors the test
                    // send below surfaces it.
                    let _ = next_frame_text(&mut ws).await;
                    shape.result(&command_id)
                }
                _ => json!({ "ok": true }),
            };
            if ws
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": result })
                        .to_string()
                        .into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(())
}

async fn next_frame_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<TcpStream>,
    >,
) -> anyhow::Result<Value> {
    let msg = ws
        .next()
        .await
        .context("frame")?
        .context("ws closed")?;
    Ok(serde_json::from_str(msg.to_text()?)?)
}

async fn recv_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<TcpStream>,
    >,
) -> anyhow::Result<Value> {
    let msg = ws
        .next()
        .await
        .context("frame")?
        .context("ws closed")?;
    Ok(serde_json::from_str(msg.to_text()?)?)
}

/// Boot hub, enroll a fake node with the given send reply, create an instance
/// and POST one instance.send, then read back the command state.
async fn send_and_state(shape: ReplyShape) -> anyhow::Result<String> {
    let dir = tempfile::tempdir()?;
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let human = hub.mint_device_token("acceptrace-human").await?;
    let host_id = "hst_acceptrace";
    let workspace_id = "wsp_acceptrace";
    spawn_fake_node(&hub, host_id, workspace_id, shape).await?;
    // Give the node.hello reconciliation a moment to register the host.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("http://{}/v1/instances", hub.addr))
        .bearer_auth(&human)
        .json(&json!({
            "hostId": host_id,
            "kind": "claude",
            "driver": "claude-sdk",
            "name": "acceptrace",
            "title": "acceptrace",
            "prompt": "create",
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_owned();

    let send: Value = http
        .post(format!(
            "http://{}/v1/instances/{instance_id}/commands",
            hub.addr
        ))
        .bearer_auth(&human)
        .json(&json!({
            "operation": "instance.send",
            "payload": { "input": { "type": "prompt", "text": "acceptrace prompt" } },
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let command_id = send["command"]["commandId"]
        .as_str()
        .or_else(|| send["command"]["id"].as_str())
        .context("response commandId")?
        .to_owned();

    // The reply and the journal append are awaited inside the Hub send, so the
    // state is settled when the POST returns; a short settle covers the bus.
    let state = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let list: Value = http
                .get(format!(
                    "http://{}/v1/instances/{instance_id}/commands?limit=10",
                    hub.addr
                ))
                .bearer_auth(&human)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let st = list["commands"]
                .as_array()
                .and_then(|rows| {
                    rows.iter().find(|r| {
                        r["id"].as_str() == Some(command_id.as_str())
                            || r["commandId"].as_str() == Some(command_id.as_str())
                    })
                })
                .and_then(|r| r["state"].as_str())
                .unwrap_or("")
                .to_owned();
            if st == "accepted" || st == "queued" || st == "reconciling" || st == "settled" {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("command state never settled")?;
    Ok(state)
}

/// The real DevNode command envelope accepts the command directly.
#[tokio::test]
async fn instance_send_with_the_real_command_envelope_is_accepted() -> anyhow::Result<()> {
    let state = send_and_state(ReplyShape::CommandEnvelope).await?;
    assert_eq!(
        state, "accepted",
        "a real-Node-shaped accepted reply must mark the command accepted; got {state}"
    );
    Ok(())
}

/// A top-level `accepted:true` (also produced by other Node operations) is
/// accepted too — guards the predicate's alternative.
#[tokio::test]
async fn instance_send_with_top_level_accepted_is_accepted() -> anyhow::Result<()> {
    let state = send_and_state(ReplyShape::TopLevelAccepted).await?;
    assert_eq!(state, "accepted", "got {state}");
    Ok(())
}

/// The pre-fix fake Node replied a bare `{ok:true}`; that is NOT a durable
/// acceptance and the row must not be marked accepted from it. This test FAILS
/// if the fake Node's bare reply is restored without the accepted envelope.
#[tokio::test]
async fn instance_send_with_bare_ok_is_not_accepted() -> anyhow::Result<()> {
    let state = send_and_state(ReplyShape::BareOk).await?;
    assert_ne!(
        state, "accepted",
        "a bare {{ok:true}} reply must never be treated as a durable acceptance; got {state}"
    );
    Ok(())
}
