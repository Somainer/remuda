//! c-acceptrace round 3 item 1: the e2e fake Node's `instance.send` arm and
//! THIS regression test run ONE shared acceptance sequence
//! (`tests/support/mod.rs`): durably `journal.append` the user message, await
//! the matching ack, then reply through the shared accepted-envelope builder.
//!
//! Before the extraction, this test ran its OWN fake Node: reverting the
//! harness arm in `examples/hub_e2e.rs` left every test here green. The guard
//! now has teeth — restoring the bare reply in the shared builder turns the
//! accepted-envelope case red.
//!
//! The fake Node is driven against the ACTUAL Hub HTTP send path over the
//! real WebSocket, and the test asserts BOTH:
//! * the exact RPC result the helper sent for `instance.send`;
//! * the settled state of the Hub's command row.

mod support;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use remuda_hub::HubConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

use support::{
    FrameQueue, InstanceSendReply, NodeWs, append_command_user, send_instance_send_rpc, send_rpc_ok,
};

/// Reply shape under test, paired with the helper variant that produces it.
#[derive(Clone, Copy)]
enum ReplyShape {
    /// The accepted-command envelope the real DevNode returns.
    AcceptedCommand,
    /// `{accepted:true}` at the result top level.
    TopLevelAccepted,
    /// The pre-fix fake-node reply.
    BareOk,
}

impl ReplyShape {
    fn helper(self) -> InstanceSendReply {
        match self {
            ReplyShape::AcceptedCommand => InstanceSendReply::AcceptedCommand,
            ReplyShape::TopLevelAccepted => InstanceSendReply::TopLevelAccepted,
            ReplyShape::BareOk => InstanceSendReply::BareOk,
        }
    }
}

/// What the test wants to inspect after the POST: the settled command state
/// plus the exact `result` object the shared helper answered the send with.
struct SendObservation {
    state: String,
    command_id: String,
    send_result: Value,
}

/// Connect a fake Node that enrolls, announces one workspace/host, and runs
/// the shared `instance.send` acceptance contract (durable append → ack →
/// reply). The exact send result is recorded in `observed`.
async fn spawn_fake_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace_id: &str,
    shape: ReplyShape,
    observed: Arc<Mutex<Option<Value>>>,
) -> anyhow::Result<()> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
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

    // Drive the WS loop until the connection is dropped by the test end. The
    // frame queue mirrors the e2e harness: frames buffered while the append
    // awaits its ack (pipelined Hub RPCs) are answered afterwards, never
    // swallowed.
    tokio::spawn(async move {
        let mut queue: FrameQueue = std::collections::VecDeque::new();
        let mut append_n = 0u64;
        let _: anyhow::Result<()> = async {
            loop {
                if queue.is_empty() {
                    match ws.next().await {
                        Some(Ok(Message::Text(text))) => queue.push_back(text.to_string()),
                        _ => return Ok(()),
                    }
                }
                while let Some(text) = queue.pop_front() {
                    let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    // A stray response (stale append ack) carries no method.
                    let Some(method) = frame.get("method").and_then(Value::as_str) else {
                        continue;
                    };
                    let id = frame.get("id").cloned().unwrap_or(Value::Null);
                    let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
                    match method {
                        "instance.create" => {
                            let iid = params["instanceId"].as_str().unwrap_or("").to_owned();
                            send_rpc_ok(
                                &mut ws,
                                id,
                                json!({ "accepted": true, "instanceId": iid }),
                            )
                            .await?;
                        }
                        "instance.send" => {
                            let iid = params["instanceId"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_default();
                            let command_id = params["commandId"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_default();
                            // The SAME durable append + ack the e2e harness
                            // runs — no test-local copy of the contract.
                            append_n = append_command_user(
                                &mut ws,
                                &mut queue,
                                &iid,
                                append_n,
                                "acceptrace prompt",
                                Some(&command_id),
                            )
                            .await?;
                            let result =
                                send_instance_send_rpc(&mut ws, id, shape.helper(), &command_id)
                                    .await?;
                            *observed.lock().await = Some(result);
                        }
                        _ => {
                            send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        }
                    }
                }
            }
        }
        .await;
    });
    Ok(())
}

async fn recv_json(ws: &mut NodeWs) -> anyhow::Result<Value> {
    let msg = ws.next().await.context("frame")?.context("ws closed")?;
    Ok(serde_json::from_str(msg.to_text()?)?)
}

/// Boot hub, enroll a fake node with the given send reply, create an instance
/// and POST one instance.send, then read back the command state and the reply
/// the helper actually sent.
async fn send_and_observe(shape: ReplyShape) -> anyhow::Result<SendObservation> {
    let dir = tempfile::tempdir()?;
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let human = hub.mint_device_token("acceptrace-human").await?;
    let host_id = "hst_acceptrace";
    let workspace_id = "wsp_acceptrace";
    let observed = Arc::new(Mutex::new(None));
    spawn_fake_node(&hub, host_id, workspace_id, shape, observed.clone()).await?;
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

    // The fake node answers strictly after the durable append ack; by the time
    // the row settled the captured reply is present.
    let send_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(result) = observed.lock().await.clone() {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("fake node never answered instance.send")?;

    Ok(SendObservation {
        state,
        command_id,
        send_result,
    })
}

/// The shared accepted-command envelope accepts the command directly, and the
/// result the helper actually sent carries that envelope for THIS command.
#[tokio::test]
async fn instance_send_with_the_shared_accepted_envelope_is_accepted() -> anyhow::Result<()> {
    let obs = send_and_observe(ReplyShape::AcceptedCommand).await?;
    assert_eq!(
        obs.state, "accepted",
        "the shared accepted envelope must mark the command accepted; got {}",
        obs.state
    );
    assert_eq!(
        obs.send_result["accepted"],
        json!(true),
        "reply: {}",
        obs.send_result
    );
    assert_eq!(
        obs.send_result["command"]["state"],
        json!("accepted"),
        "reply: {}",
        obs.send_result
    );
    assert_eq!(
        obs.send_result["command"]["commandId"],
        json!(obs.command_id),
        "reply: {}",
        obs.send_result
    );
    Ok(())
}

/// A top-level `accepted:true` (also produced by other Node operations) is
/// accepted too — guards the predicate's alternative.
#[tokio::test]
async fn instance_send_with_top_level_accepted_is_accepted() -> anyhow::Result<()> {
    let obs = send_and_observe(ReplyShape::TopLevelAccepted).await?;
    assert_eq!(obs.state, "accepted", "got {}", obs.state);
    assert_eq!(obs.send_result["accepted"], json!(true));
    Ok(())
}

/// The pre-fix fake Node replied a bare `{ok:true}`; that is NOT a durable
/// acceptance and the row must not be marked accepted from it.
#[tokio::test]
async fn instance_send_with_bare_ok_is_not_accepted() -> anyhow::Result<()> {
    let obs = send_and_observe(ReplyShape::BareOk).await?;
    assert_ne!(
        obs.state, "accepted",
        "a bare {{ok:true}} reply must never be treated as a durable acceptance; got {}",
        obs.state
    );
    assert!(
        obs.send_result.get("accepted").is_none(),
        "reply: {}",
        obs.send_result
    );
    Ok(())
}
