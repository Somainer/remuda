//! c-acceptrace round 4: the accepting `instance.send` case and the e2e
//! harness run the SAME function — [`support::accept_instance_send`]:
//! durable `journal.append` of the user message, await the MATCHING ack
//! (failing on an RPC error), and answer the Hub RPC with the REAL typed
//! `CommandResult` (no hand-written envelope exists anywhere).
//!
//! * accepting case — goes through the shared function, which is also the
//!   harness's only send-accept path; reverting its reply builder to
//!   `{ok:true}` (or splitting the append so the ack is skipped) reddens this
//!   file;
//! * top-level `accepted:true` and bare `{ok:true}` stay as separate
//!   predicate-regression cases (durable append, alternative reply);
//! * seq-gap case — a rejected append is carried to the send RPC as an error,
//!   the row settles rejected, and no message is durable;
//! * two loopback unit tests prove ack matching by id and error surfacing.

mod support;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use remuda_hub::HubConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::client::IntoClientRequest};

use support::{
    AckFrame, FrameQueue, InstanceSendReply, NodeWs, accept_instance_send, await_ack_frame,
    durable_command_append, send_instance_send_rpc, send_rpc_error, send_rpc_ok,
};

const PROMPT: &str = "acceptrace prompt";
/// Seq hint safely past the durable cursor on a fresh instance, so the Hub
/// rejects the append with a journal gap (store.rs append_loaded_event).
const GAP_SEQ: i64 = 50;

/// How the fake Node plays the one `instance.send` in a scenario.
#[derive(Clone, Copy)]
enum FakeBehavior {
    /// The ONE shared acceptance contract the e2e harness runs:
    /// [`accept_instance_send`] — durable append, matched ack, REAL typed
    /// `CommandResult` reply.
    AcceptShared,
    /// Durable append succeeds; answer with this non-acceptance result shape
    /// (predicate-alternative regression cases).
    Reply(InstanceSendReply),
    /// Force a seq-gap append error. A contract-correct Node maps that to an
    /// RPC error for the send; one that ignores the append error would instead
    /// answer accepted — the false acceptance this scenario guards.
    GapRejectsSend,
}

/// Everything the test inspects after the POST.
struct SendObservation {
    command_id: String,
    /// Raw POST `/commands` response body.
    post: Value,
    /// Settled row state from GET `.../commands`.
    state: String,
    /// Settlement projection from GET `.../commands` (absent unless settled).
    settlement: Value,
    /// Result object the fake Node answered instance.send with.
    send_result: Option<Value>,
    /// The validated durable append ack (success scenarios only).
    append_ack: Option<Value>,
    /// The journal.append rejection message (gap scenario only).
    append_error: Option<String>,
    /// GET `.../journal` response body.
    journal: Value,
}

/// Mutable record the spawned fake Node writes the test reads.
#[derive(Default)]
struct FakeRecord {
    send_result: Option<Value>,
    append_ack: Option<Value>,
    append_error: Option<String>,
}

/// Connect a fake Node that enrolls, announces one workspace/host, and runs
/// the shared `instance.send` contract from `support`.
async fn spawn_fake_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace_id: &str,
    behavior: FakeBehavior,
    record: Arc<Mutex<FakeRecord>>,
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

    // The frame queue mirrors the e2e harness: frames buffered while the
    // append awaits its ack (pipelined Hub RPCs) are answered afterwards,
    // never swallowed.
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
                            match behavior {
                                FakeBehavior::AcceptShared => {
                                    // The EXACT function the e2e harness calls:
                                    // durable append + matched ack + the real
                                    // CommandResult reply.
                                    let accepted = accept_instance_send(
                                        &mut ws,
                                        &mut queue,
                                        id,
                                        &iid,
                                        append_n,
                                        PROMPT,
                                        &command_id,
                                        None,
                                    )
                                    .await?;
                                    append_n = accepted.append_seq;
                                    let mut rec = record.lock().await;
                                    rec.append_ack = Some(accepted.append_ack);
                                    rec.send_result = Some(accepted.send_result);
                                }
                                FakeBehavior::Reply(shape) => {
                                    // Same durable append half; only the RPC
                                    // result shape differs (predicate cases).
                                    let (new_n, ack) = durable_command_append(
                                        &mut ws,
                                        &mut queue,
                                        &iid,
                                        append_n,
                                        PROMPT,
                                        Some(&command_id),
                                        None,
                                    )
                                    .await?;
                                    append_n = new_n;
                                    let result = send_instance_send_rpc(&mut ws, id, shape).await?;
                                    let mut rec = record.lock().await;
                                    rec.append_ack = Some(ack);
                                    rec.send_result = Some(result);
                                }
                                FakeBehavior::GapRejectsSend => {
                                    // The SAME shared function, with a seq hint
                                    // past the durable cursor: the append must
                                    // fail, no accepted reply is sent, and the
                                    // Node carries the error to the send RPC.
                                    match accept_instance_send(
                                        &mut ws,
                                        &mut queue,
                                        id.clone(),
                                        &iid,
                                        append_n,
                                        PROMPT,
                                        &command_id,
                                        Some(GAP_SEQ),
                                    )
                                    .await
                                    {
                                        Ok(accepted) => anyhow::bail!(
                                            "fixture: expected a journal-gap error, got {}",
                                            accepted.send_result
                                        ),
                                        Err(error) => {
                                            let message = error.to_string();
                                            send_rpc_error(
                                                &mut ws,
                                                id,
                                                -32000,
                                                &format!(
                                                    "node did not accept instance.send: {message}"
                                                ),
                                            )
                                            .await?;
                                            record.lock().await.append_error = Some(message);
                                        }
                                    }
                                }
                            }
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

/// Find the message event journaled for `command_id`, if any. GET journal
/// rows nest the wire event under `event` alongside the stored seq/eventId.
fn journal_command_message<'a>(journal: &'a Value, command_id: &str) -> Option<&'a Value> {
    journal["events"]
        .as_array()?
        .iter()
        .map(|row| row.get("event").unwrap_or(row))
        .find(|event| {
            event.get("kind").and_then(Value::as_str) == Some("message")
                && event.pointer("/payload/commandId").and_then(Value::as_str) == Some(command_id)
        })
}

/// Boot hub, enroll a fake node with the given behavior, create an instance,
/// POST one instance.send, settle the row, and capture the reply, ack, row and
/// journal.
async fn run_scenario(behavior: FakeBehavior) -> anyhow::Result<SendObservation> {
    let dir = tempfile::tempdir()?;
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let human = hub.mint_device_token("acceptrace-human").await?;
    let host_id = "hst_acceptrace";
    let workspace_id = "wsp_acceptrace";
    let record = Arc::new(Mutex::new(FakeRecord::default()));
    spawn_fake_node(&hub, host_id, workspace_id, behavior, record.clone()).await?;
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

    let post: Value = http
        .post(format!(
            "http://{}/v1/instances/{instance_id}/commands",
            hub.addr
        ))
        .bearer_auth(&human)
        .json(&json!({
            "operation": "instance.send",
            "payload": { "input": { "type": "prompt", "text": PROMPT } },
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let command_id = post["command"]["commandId"]
        .as_str()
        .or_else(|| post["command"]["id"].as_str())
        .context("response commandId")?
        .to_owned();

    // Wait for the row to reach a terminal progress state.
    let (state, settlement) = tokio::time::timeout(Duration::from_secs(5), async {
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
            if let Some(row) = list["commands"].as_array().and_then(|rows| {
                rows.iter().find(|r| {
                    r["id"].as_str() == Some(command_id.as_str())
                        || r["commandId"].as_str() == Some(command_id.as_str())
                })
            }) {
                let st = row["state"].as_str().unwrap_or("").to_owned();
                // `queued`/`reconciling` are terminal FOR THIS OBSERVATION: a
                // bare-ok reply deliberately leaves the row resting for
                // journal convergence rather than marking it accepted.
                if st == "accepted" || st == "queued" || st == "reconciling" || st == "settled" {
                    return (st, row.get("settlement").cloned().unwrap_or(Value::Null));
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("command state never settled")?;

    let journal: Value = http
        .get(format!(
            "http://{}/v1/instances/{instance_id}/journal",
            hub.addr
        ))
        .bearer_auth(&human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let rec = record.lock().await;
    Ok(SendObservation {
        command_id,
        post,
        state,
        settlement,
        send_result: rec.send_result.clone(),
        append_ack: rec.append_ack.clone(),
        append_error: rec.append_error.clone(),
        journal,
    })
}

/// Assert the durable append ack and the durable journal message for the
/// command agree (success scenarios).
fn assert_durable_message(obs: &SendObservation) {
    let ack = obs
        .append_ack
        .as_ref()
        .expect("a successful send records the durable append ack");
    assert!(
        ack.get("seq")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "append ack names a seq: {ack}"
    );
    assert!(
        ack.get("eventId")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "append ack names an eventId: {ack}"
    );
    let message = journal_command_message(&obs.journal, &obs.command_id).unwrap_or_else(|| {
        panic!(
            "the acked user message must be durable in the journal: {}",
            obs.journal
        )
    });
    assert_eq!(
        message["eventId"], ack["eventId"],
        "journal event: {message}"
    );
    assert_eq!(message["seq"], ack["seq"], "journal event: {message}");
    assert_eq!(
        message.pointer("/payload/role").and_then(Value::as_str),
        Some("user"),
        "journal event: {message}"
    );
}

/// A real DevNode `instance.send` result is a typed `CommandResult` —
/// `{command, relatedCommandIds}` with NO top-level `accepted`/`ok`. The Hub
/// must accept from `/command/state` alone (dropping that arm reddens only
/// this case), and the user message is durable with the acked event id.
#[tokio::test]
async fn instance_send_with_a_real_command_result_is_accepted() -> anyhow::Result<()> {
    let obs = run_scenario(FakeBehavior::AcceptShared).await?;
    assert_eq!(
        obs.state, "accepted",
        "a real CommandResult with command.state=accepted must accept; got {}",
        obs.state
    );
    assert_eq!(obs.post["command"]["state"], json!("accepted"));
    let result = obs.send_result.as_ref().expect("send result captured");
    assert!(result.get("accepted").is_none(), "reply: {result}");
    assert!(result.get("ok").is_none(), "reply: {result}");
    assert!(result["relatedCommandIds"].is_array(), "reply: {result}");
    assert_eq!(result["command"]["state"], json!("accepted"));
    assert_eq!(result["command"]["commandId"], json!(obs.command_id));
    assert_eq!(result["command"]["operation"], json!("instance.send"));
    assert_durable_message(&obs);
    Ok(())
}

/// A top-level `accepted:true` (produced by other Node operations) is
/// accepted too — guards the predicate's alternative; the durable contract
/// is identical.
#[tokio::test]
async fn instance_send_with_top_level_accepted_is_accepted() -> anyhow::Result<()> {
    let obs = run_scenario(FakeBehavior::Reply(InstanceSendReply::TopLevelAccepted)).await?;
    assert_eq!(obs.state, "accepted", "got {}", obs.state);
    assert_eq!(
        obs.send_result.as_ref().expect("send result captured")["accepted"],
        json!(true)
    );
    assert_durable_message(&obs);
    Ok(())
}

/// The pre-fix fake Node replied a bare `{ok:true}`; that is NOT a durable
/// acceptance. The user message is still durable (the append happened), but
/// the RPC alone must not mark the row accepted.
#[tokio::test]
async fn instance_send_with_bare_ok_is_not_accepted() -> anyhow::Result<()> {
    let obs = run_scenario(FakeBehavior::Reply(InstanceSendReply::BareOk)).await?;
    assert_ne!(
        obs.state, "accepted",
        "a bare {{ok:true}} reply must never be treated as acceptance; got {}",
        obs.state
    );
    let result = obs.send_result.as_ref().expect("send result captured");
    assert!(result.get("accepted").is_none(), "reply: {result}");
    assert_durable_message(&obs);
    Ok(())
}

/// When the durable append is rejected (journal seq gap), the Node must carry
/// that failure to the instance.send RPC instead of answering accepted: the
/// command settles rejected, the failed message is NOT in the journal, and the
/// rejection message names the gap. Ignoring the jappend error and replying
/// "accepted" anyway (the pre-fix behavior) fails this test.
#[tokio::test]
async fn instance_send_after_a_rejected_append_settles_rejected() -> anyhow::Result<()> {
    let obs = run_scenario(FakeBehavior::GapRejectsSend).await?;
    // The row must settle rejected — never be marked accepted from an
    // instance.send reply that ignored the failed append.
    assert_eq!(
        obs.post["command"]["state"],
        json!("settled"),
        "POST row: {}",
        obs.post
    );
    assert_eq!(obs.state, "settled", "commands GET state: {}", obs.state);
    assert_eq!(
        obs.settlement["outcome"],
        json!("rejected"),
        "settlement: {}",
        obs.settlement
    );
    let error = obs
        .append_error
        .as_ref()
        .expect("the fake node must record the append rejection");
    assert!(
        error.contains("journal gap"),
        "expected a journal-gap rejection, got: {error}"
    );
    assert!(
        obs.send_result.is_none(),
        "no accepted result may be sent after a rejected append: {:?}",
        obs.send_result
    );
    assert!(
        journal_command_message(&obs.journal, &obs.command_id).is_none(),
        "a rejected append must leave no durable user message: {}",
        obs.journal
    );
    Ok(())
}

/// Tiny loopback WebSocket server that sends `frames` to the one connecting
/// client, then parks with the socket open.
async fn frame_server(frames: Vec<Value>) -> anyhow::Result<NodeWs> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut server) = accept_async(stream).await
        {
            for frame in frames {
                if server
                    .send(Message::Text(frame.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            // Park so buffered frames finish draining; ends when the client
            // drops at test teardown.
            let _ = server.next().await;
        }
    });
    let (ws, _) = connect_async(format!("ws://{addr}")).await?;
    Ok(ws)
}

/// Waiting for an append ack matches by JSON-RPC id and QUEUES unrelated
/// frames (a pipelined Hub RPC) instead of swallowing or mistaking them for
/// the ack.
#[tokio::test]
async fn ack_wait_matches_by_id_and_keeps_unrelated_frames() -> anyhow::Result<()> {
    let mut ws = frame_server(vec![
        json!({"id": "rpc-42", "method": "interaction.list", "params": {}}),
        json!({
            "id": "j1",
            "result": {"seq": "1", "eventId": "evt_1", "durableSeq": "1"}
        }),
    ])
    .await?;
    let mut queue: FrameQueue = std::collections::VecDeque::new();
    match await_ack_frame(&mut ws, &mut queue, "j1").await? {
        AckFrame::Accepted(result) => {
            assert_eq!(result["seq"], json!("1"));
        }
        AckFrame::Rejected(value) => panic!("expected success ack, got error: {value}"),
    }
    assert_eq!(queue.len(), 1, "the unrelated RPC must stay queued");
    let kept: Value = serde_json::from_str(queue.front().expect("queued frame"))?;
    assert_eq!(kept["id"], json!("rpc-42"));
    assert_eq!(kept["method"], json!("interaction.list"));
    Ok(())
}

/// An id-matched `error` response is surfaced as a rejection, not a success.
#[tokio::test]
async fn ack_wait_surfaces_an_rpc_error() -> anyhow::Result<()> {
    let mut ws = frame_server(vec![json!({
        "id": "j7",
        "error": {"code": -32000, "message": "journal gap: expected 1, got 50"}
    })])
    .await?;
    let mut queue: FrameQueue = std::collections::VecDeque::new();
    match await_ack_frame(&mut ws, &mut queue, "j7").await? {
        AckFrame::Rejected(value) => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap();
            assert!(message.contains("journal gap"), "message: {message}");
        }
        AckFrame::Accepted(value) => panic!("expected error ack, got success: {value}"),
    }
    assert!(queue.is_empty());
    Ok(())
}
