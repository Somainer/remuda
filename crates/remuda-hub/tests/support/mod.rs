//! Shared fake-Node machinery for the `hub_e2e` example harness and the
//! `accept_reply` regression test.
//!
//! The fake Node's `instance.send` acceptance lives in ONE place here —
//! [`accept_instance_send`]: durably `journal.append` the user message, await
//! the MATCHING append ack (queuing unrelated frames instead of swallowing
//! them, failing on an RPC error), and answer the Hub RPC with the REAL typed
//! `CommandResult` ([`real_command_result`]). Both the e2e harness and the
//! regression call that exact function, so neither copy can drift back to the
//! old hand-written envelope or the discarded ack without reddening the test.

// This file is compiled as part of TWO binaries (the hub_e2e example and the
// accept_reply integration test), each of which uses a different subset of
// the fake-Node helpers; items unused in one are live in the other.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

/// WebSocket the fake Node drives the Hub over.
pub(crate) type NodeWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// Frames stashed while an append waits for its own ack. Every journal.append
/// waits on its JSON-RPC id; a Hub RPC (instance.send, interaction.list,
/// tty.screen, …) that arrives meanwhile is pushed here so the main dispatch
/// loop answers it — never swallowed by the ack wait.
pub(crate) type FrameQueue = std::collections::VecDeque<String>;

/// Total time one journal append may take to be acknowledged. This is an
/// ABSOLUTE budget: it must not reset per received frame, or a steady trickle
/// of unrelated frames (interaction.list polls every ~2 s, pipelined RPCs)
/// would let a missing ack park the Node in the append handler forever and
/// grow the stash without bound.
pub(crate) const FRAME_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on frames stashed behind one ack wait. Hitting it is a LOUD harness
/// failure, never an unbounded queue.
pub(crate) const PENDING_FRAME_QUEUE_CAP: usize = 4096;

/// Answer a Hub RPC with a successful result.
pub(crate) async fn send_rpc_ok(ws: &mut NodeWs, id: Value, result: Value) -> Result<()> {
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
            .to_string()
            .into(),
    ))
    .await?;
    Ok(())
}

/// Answer a Hub RPC with a JSON-RPC error. The fake Node uses this when the
/// durable intent (journal.append) failed: the real Node never answers
/// `accepted` after a rejected durable write.
pub(crate) async fn send_rpc_error(
    ws: &mut NodeWs,
    id: Value,
    code: i64,
    message: &str,
) -> Result<()> {
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            .to_string()
            .into(),
    ))
    .await?;
    Ok(())
}

/// Classify a frame against the wanted append-ack id.
///
/// - `Some(Ok(()))`  — a matching SUCCESS response (object `result`, no error)
/// - `Some(Err(_))`  — a matching response carrying an `error`, or no result
///   object (a rejected/failed append must never be hidden as success)
/// - `None`          — not the wanted response (a Hub RPC or a stale ack)
pub(crate) fn classify_ack_frame(value: &Value, want: &str) -> Option<Result<()>> {
    if value.get("method").is_some() {
        return None;
    }
    if value.get("id").and_then(Value::as_str) != Some(want) {
        return None;
    }
    if let Some(error) = value.get("error") {
        return Some(Err(anyhow!("append ack {want} was an RPC error: {error}")));
    }
    if !value.get("result").is_some_and(Value::is_object) {
        return Some(Err(anyhow!(
            "append ack {want} carried no result object: {value}"
        )));
    }
    Some(Ok(()))
}

/// Terminal classification of an id-matched append ack.
pub(crate) enum AckFrame {
    /// Matching SUCCESS response; carries the result object so the caller can
    /// validate the durable ack (`seq` / `eventId` / `durableSeq`).
    Accepted(Value),
    /// Matching response carrying an `error` object. The Hub returns seq-gap
    /// rejections this way WITHOUT closing the socket (ws.rs journal.append
    /// error arm), so a fake Node that ignores this still has a live link.
    Rejected(Value),
}

/// Read frames until the Hub acknowledges the journal append with id `want`.
///
/// Uses ONE absolute deadline ([`FRAME_ACK_TIMEOUT`] from the first call),
/// blocking only for the time remaining each iteration. Before blocking it
/// scans `queue` for an already-buffered matching ack (the determinism
/// barrier and nested appends can stash the ack ahead of unrelated RPCs).
///
/// Every other frame — a stale fire-and-forget append ack or an unrelated Hub
/// RPC like the web poll's interaction.list — is stashed in `queue` for the
/// main loop to process, so waiting on durability can never swallow an RPC.
/// A missing ack or an over-capacity queue is a loud error; an id-matched
/// response is handed back as [`AckFrame`] for the caller to act on.
pub(crate) async fn await_ack_frame(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    want: &str,
) -> Result<AckFrame> {
    let deadline = Instant::now() + FRAME_ACK_TIMEOUT;
    loop {
        // 1. Already-buffered match (the ack may be queued ahead of RPCs).
        if let Some(pos) = queue.iter().position(|frame| {
            serde_json::from_str::<Value>(frame)
                .map(|v| classify_ack_frame(&v, want).is_some())
                .unwrap_or(false)
        }) {
            let frame = queue.remove(pos).context("frame queue position vanished")?;
            let value: Value = serde_json::from_str(&frame)?;
            return Ok(classify_matched_ack(value, want));
        }
        // 2. Block for the REMAINING time only — the deadline never extends.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "timed out after {} ms waiting for append ack {want} ({} frame(s) queued)",
                FRAME_ACK_TIMEOUT.as_millis(),
                queue.len()
            );
        }
        let frame = match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(_) => anyhow::bail!("hub connection closed waiting for append ack {want}"),
            Err(_) => {
                anyhow::bail!(
                    "timed out after {} ms waiting for append ack {want} ({} frame(s) queued)",
                    FRAME_ACK_TIMEOUT.as_millis(),
                    queue.len()
                )
            }
        };
        let value: Value = serde_json::from_str(&frame)?;
        if classify_ack_frame(&value, want).is_none() {
            if queue.len() >= PENDING_FRAME_QUEUE_CAP {
                anyhow::bail!(
                    "pending frame queue overflow (>{PENDING_FRAME_QUEUE_CAP}) waiting for append ack {want}"
                );
            }
            queue.push_back(frame.to_string());
            continue;
        }
        return Ok(classify_matched_ack(value, want));
    }
}

/// Split an already id-matched ack frame into success/error, handing the
/// caller the result or error object instead of stringifying it.
fn classify_matched_ack(value: Value, want: &str) -> AckFrame {
    match classify_ack_frame(&value, want).expect("frame just matched") {
        Ok(()) => AckFrame::Accepted(
            value
                .get("result")
                .cloned()
                .expect("classify_ack_frame required an object result"),
        ),
        Err(_) => AckFrame::Rejected(value),
    }
}

/// Await the matching append ack and treat an `error` response as a loud
/// error (the e2e harness contract).
pub(crate) async fn wait_frame_ack(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    want: &str,
) -> Result<()> {
    match await_ack_frame(ws, queue, want).await? {
        AckFrame::Accepted(_) => Ok(()),
        AckFrame::Rejected(value) => Err(anyhow!(
            "append ack {want} was an RPC error: {}",
            value.get("error").unwrap_or(&value)
        )),
    }
}

/// How the shared durable append waits for its ack.
#[derive(Clone)]
pub(crate) enum AckBarrier {
    /// Consume the ack as soon as it arrives.
    Immediate,
    /// c-hubfakeack determinism barrier: do not consume the append ack until a
    /// FURTHER Hub→Node RPC (the web poll's `interaction.list`) has actually
    /// arrived on the socket and been stashed behind it. This makes the "RPC
    /// buffered behind an outstanding ack" race structural instead of relying
    /// on scheduler timing, while keeping the SAME strict ack validation as
    /// the immediate path.
    InterveningRpc {
        /// Hub port; names the queue marker file (`remuda-e2e-ackqueue-<port>`).
        port: u16,
        /// Receives the buffered RPC id so the dispatch loop's
        /// interaction.list arm can write its REPLY marker.
        tracked: Arc<Mutex<Option<Value>>>,
    },
}

/// How long the deterministic barrier waits for the intervening RPC.
/// Deliberately below the Hub's command-accept budget: a missed RPC fails
/// loudly in the Node before the Hub gives up and returns reconciling.
pub(crate) const ACK_BARRIER_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Test-only negative control: HUB_E2E_ACK_DISCARD=1 makes the barrier RECORD
/// the intervening interaction.list (and its marker) but DROP the frame,
/// reproducing the old swallowed-frame bug while keeping the new markers.
pub(crate) fn ack_discard_enabled() -> bool {
    std::env::var("HUB_E2E_ACK_DISCARD").as_deref() == Ok("1")
}

/// The interaction.list frame the ack barrier is waiting for, if `frame` is
/// one: returns its parsed JSON-RPC id.
fn interaction_list_id(frame: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(frame).ok()?;
    if value.get("method").and_then(Value::as_str) == Some("interaction.list") {
        value.get("id").cloned()
    } else {
        None
    }
}

/// Park behind the append ack `want` until an `interaction.list` RPC is
/// buffered (the spec issues that GET itself). Records the RPC's id in
/// `tracked` and writes the QUEUE marker (`<id> interaction.list`) the moment
/// the frame arrives. Under HUB_E2E_ACK_DISCARD=1 the frame is recorded but
/// then DROPPED (negative control): the interaction.list arm can never answer
/// it, so the separate REPLY marker never appears even though the queue marker
/// does — exactly the gap this regression must catch.
pub(crate) async fn ensure_intervening_rpc(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    want: &str,
    port: u16,
    tracked: Arc<Mutex<Option<Value>>>,
) -> Result<()> {
    let queue_marker = std::env::temp_dir().join(format!("remuda-e2e-ackqueue-{port}"));
    let record = |rpc_id: &Value| {
        let body = format!("{rpc_id} interaction.list");
        std::fs::write(&queue_marker, body)
            .with_context(|| format!("write ack queue marker {}", queue_marker.display()))
    };

    // 1. Already buffered ahead of the wait; remember its QUEUE POSITION so the
    // discard negative control can remove THAT entry (removing only a clone
    // would leave the original queued, answered, and wrongly reply-marked).
    let mut found: Option<(Value, String, Option<usize>)> = queue
        .iter()
        .position(|f| interaction_list_id(f).is_some())
        .map(|pos| {
            let frame = &queue[pos];
            (
                interaction_list_id(frame).expect("just matched"),
                frame.clone(),
                Some(pos),
            )
        });

    let deadline = Instant::now() + ACK_BARRIER_TIMEOUT;
    while found.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "ack barrier: no interaction.list arrived behind append ack {want} within {} ms",
                ACK_BARRIER_TIMEOUT.as_millis()
            );
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let text = text.to_string();
                if interaction_list_id(&text).is_some() {
                    // A frame just read: not in the queue yet (position None).
                    let id = interaction_list_id(&text).expect("matched above");
                    found = Some((id, text, None));
                } else if queue.len() < PENDING_FRAME_QUEUE_CAP {
                    // A journal ack or other RPC: stash for the dispatch loop.
                    queue.push_back(text);
                } else {
                    anyhow::bail!("pending frame queue overflow in ack barrier for {want}");
                }
            }
            Ok(_) => anyhow::bail!("hub connection closed during ack barrier for {want}"),
            Err(_) => {
                anyhow::bail!(
                    "ack barrier: no interaction.list arrived behind append ack {want} within {} ms",
                    ACK_BARRIER_TIMEOUT.as_millis()
                )
            }
        }
    }

    let (rpc_id, frame, queued_pos) = found.expect("an interaction.list was observed");
    record(&rpc_id)?;
    *tracked.lock().await = Some(rpc_id);
    if ack_discard_enabled() {
        // NEGATIVE CONTROL: record + marker written, then REMOVE the frame for
        // real. If it was already queued, drop the entry at its captured
        // position; otherwise simply do not enqueue the freshly read frame.
        if let Some(pos) = queued_pos {
            if pos < queue.len() && queue[pos] == frame {
                queue.remove(pos);
            } else if let Some(eq) = queue.iter().position(|f| f == &frame) {
                queue.remove(eq);
            }
        }
        tracing::warn!("HUB_E2E_ACK_DISCARD=1: dropped queued interaction.list behind {want}");
    } else if queued_pos.is_none() && queue.len() < PENDING_FRAME_QUEUE_CAP {
        // Normal mode, freshly read frame: enqueue it, preserving order.
        queue.push_back(frame);
    }
    // Normal mode with queued_pos = Some: the frame is already in the queue at
    // its original position; leave it there (do not move it to the back).
    Ok(())
}

/// Derive the user node id a command-delivered prompt is journaled under:
/// `obj_<uuid>` for a `cmd_<uuid>` command, falling back to a per-seq legacy
/// id when the command id is missing or unexpectedly shaped.
pub(crate) fn command_user_node(command_id: Option<&str>, n: u64) -> String {
    command_id
        .map(|id| {
            id.strip_prefix("cmd_")
                .map_or_else(|| format!("obj_node_{n}"), |uuid| format!("obj_{uuid}"))
        })
        .unwrap_or_else(|| format!("obj_legacy_{n}"))
}

/// One full-shape user message observation. A composer/command send carries
/// `command_id` (C2 correlation); a natively typed prompt omits it.
/// Send one command/user journal message frame and return its `j{seq}` id;
/// does NOT wait for the append ack (callers drive [`wait_frame_ack`],
/// possibly behind the determinism barrier).
pub(crate) async fn send_user_message_frame(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
    node: &str,
) -> Result<u64> {
    send_user_message_frame_with_seq(ws, instance_id, n, text, command_id, node, None).await
}

/// Like [`send_user_message_frame`] but pins the frame's `seq` hint. A hint
/// past the durable cursor makes the Hub reject the append with a journal-gap
/// RPC error (store.rs append_loaded_event) WITHOUT closing the socket — the
/// fault the item-3 regression drives.
pub(crate) async fn send_user_message_frame_with_seq(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
    node: &str,
    seq_hint: Option<i64>,
) -> Result<u64> {
    let seq = n + 1;
    let mut payload = json!({
        "nodeId": node,
        "messageId": node,
        "revision": "1",
        "operation": "open",
        "role": "user",
        "phase": "input",
        "blocks": [{ "type": "text", "text": text }],
        "targetBlock": null,
        "parentToolCallId": null,
        "nativeOrigin": { "state": "known", "value": "ui" },
        "origin": "human",
        "status": "complete",
    });
    if let Some(command_id) = command_id {
        payload["commandId"] = json!(command_id);
    }
    let mut params = json!({
        "instanceId": instance_id,
        "event": {
            "kind": "message",
            "completeness": "structured",
            "payload": payload,
        }
    });
    if let Some(hint) = seq_hint {
        params["seq"] = json!(hint);
    }
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": format!("j{seq}"),
            "method": "journal.append",
            "params": params,
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(seq)
}

/// Send one command/user journal message frame and await its durable ack.
pub(crate) async fn append_user_message(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
    node: &str,
) -> Result<u64> {
    let seq = send_user_message_frame(ws, instance_id, n, text, command_id, node).await?;
    wait_frame_ack(ws, queue, &format!("j{seq}")).await?;
    Ok(seq)
}

/// A command-delivered prompt: its user node carries the delivering commandId.
pub(crate) async fn append_command_user(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
) -> Result<u64> {
    let node = command_user_node(command_id, n);
    append_user_message(ws, queue, instance_id, n, text, command_id, &node).await
}

/// The durable half of the fake Node's `instance.send` contract: append the
/// command's user message, await the MATCHING `j{seq}` response id, FAIL on an
/// RPC error, and validate the success ack's durable fields. Returns the
/// validated ack result object (`seq` / `eventId` / `durableSeq`) and the
/// frame seq, so a caller can correlate the durable journal message.
///
/// `barrier` optionally forces a further Hub RPC to be buffered behind the
/// outstanding ack FIRST (the deterministic c-hubfakeack interleaving) — the
/// strict ack validation is identical either way.
///
/// A caller that gets `Err` here must answer `instance.send` with an RPC
/// error, never an accepted envelope — the intent was not durable.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn durable_command_append(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
    seq_hint: Option<i64>,
    barrier: &AckBarrier,
) -> Result<(u64, Value)> {
    let node = command_user_node(command_id, n);
    let seq =
        send_user_message_frame_with_seq(ws, instance_id, n, text, command_id, &node, seq_hint)
            .await?;
    let want = format!("j{seq}");
    if let AckBarrier::InterveningRpc { port, tracked } = barrier {
        ensure_intervening_rpc(ws, queue, &want, *port, tracked.clone()).await?;
    }
    let ack = match await_ack_frame(ws, queue, &want).await? {
        AckFrame::Accepted(result) => result,
        AckFrame::Rejected(response) => {
            let message = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown journal.append error");
            anyhow::bail!("journal.append {want} rejected: {message}");
        }
    };
    // Validate the durable ack: a fresh append names its seq and event id, and
    // the durable cursor is at least that far.
    let ack_seq = ack
        .get("seq")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
        .with_context(|| format!("append ack {want} carried no numeric seq: {ack}"))?;
    let durable = ack
        .get("durableSeq")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
        .with_context(|| format!("append ack {want} carried no numeric durableSeq: {ack}"))?;
    if ack
        .get("eventId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        anyhow::bail!("append ack {want} carried no eventId: {ack}");
    }
    if durable < ack_seq {
        anyhow::bail!("append ack {want} regressed: durableSeq {durable} < seq {ack_seq}: {ack}");
    }
    Ok((seq, ack))
}

/// Serialize the REAL `instance.send` result a DevNode returns: a typed
/// `CommandResult` = `{command, relatedCommandIds}` for an accepted command —
/// NO top-level `accepted` or `ok` (the real Node never sends those;
/// `DevNode::submit_command` in remuda-node builds exactly this shape). The
/// Hub must therefore accept from `/command/state` alone.
///
/// The value is produced by serde from the protocol structs rather than a JSON
/// literal, so the case tracks the real wire schema (field names, nesting,
/// enum spellings). This is the SINGLE accepted-reply builder shared by the
/// e2e harness and the regression: reverting it to a bare `{ok:true}` reddens
/// the accepting test while leaving every other path untouched.
pub(crate) fn real_command_result(command_id: &str, instance_id: &str) -> Result<Value> {
    use remuda_protocol::{
        Acceptance, AcceptanceScope, ActorRef, ActorType, Command as ProtocolCommand,
        CommandAuthority, CommandId, CommandOperation, CommandOrigin, CommandResult, CommandState,
        CommandTarget, DispatchState, EntityMeta, ExpectedState, HostId, Id, InstanceId, Knowledge,
        NodeReceipt, ResolutionState, Timestamp, U64,
    };

    /// Fixed, schema-valid RFC3339-ms timestamp for the fixture (the Hub never
    /// compares the embedded clock).
    const FIXED_NOW: &str = "2026-01-01T00:00:00.000Z";

    let id = CommandId::try_from(command_id.to_owned())
        .with_context(|| format!("fixture command id {command_id} must be a branded cmd_ id"))?;
    let instance_id = InstanceId::try_from(instance_id.to_owned())
        .with_context(|| format!("fixture instance id {instance_id} must be a branded ins_ id"))?;
    let host_id = HostId::try_from(String::from(Id::new("hst")?))?;
    let now: Timestamp = FIXED_NOW.to_owned().try_into()?;
    let digest = format!("sha256:{}", "0".repeat(64)).try_into()?;
    let command = ProtocolCommand {
        meta: EntityMeta {
            id: id.clone(),
            revision: U64(2),
            created_at: now.clone(),
            updated_at: now.clone(),
        },
        command_id: id,
        actor: ActorRef {
            principal_id: Id::new("prn")?,
            actor_type: ActorType::Human,
            device_id: None,
            instance_id: Some(instance_id.clone()),
        },
        origin: CommandOrigin::Ui,
        operation: CommandOperation::InstanceSend,
        target: CommandTarget {
            host_id,
            instance_id: Some(instance_id),
            run_id: None,
        },
        payload_ref: Id::new("obj")?,
        payload_digest: digest,
        expected: ExpectedState {
            instance_revision: None,
            process_generation: Some(U64(1)),
            run_generation: None,
            owner_fence: Some(U64(1)),
            interaction_version: None,
        },
        state: CommandState::Accepted,
        authority: CommandAuthority::NodeLedger,
        forward_intent: Knowledge::NotApplicable,
        dispatch: DispatchState::IntentDurable,
        resolution: ResolutionState::Clear,
        acceptance: Knowledge::Known {
            value: Acceptance {
                scope: AcceptanceScope::NativeInput,
                event_ids: Vec::new(),
            },
        },
        settlement: Knowledge::NotApplicable,
        queued_at: now.clone(),
        accepted_at: Knowledge::Known { value: now },
        settled_at: Knowledge::NotApplicable,
        expires_at: None,
        node_receipt: Knowledge::Known {
            value: NodeReceipt {
                node_epoch: Id::new("epoch")?,
                ledger_revision: U64(1),
            },
        },
    };
    let result = CommandResult {
        command,
        related_command_ids: Vec::new(),
    };
    Ok(serde_json::to_value(result)?)
}

/// Send the REAL accepted `CommandResult` for one `instance.send` RPC;
/// returns the exact result object sent so a caller can correlate it.
pub(crate) async fn send_command_result_reply(
    ws: &mut NodeWs,
    rpc_id: Value,
    command_id: &str,
    instance_id: &str,
) -> Result<Value> {
    let result = real_command_result(command_id, instance_id)?;
    send_rpc_ok(ws, rpc_id, result.clone()).await?;
    Ok(result)
}

/// Everything [`accept_instance_send`] produced, so a caller (the regression)
/// can assert every half of the acceptance contract.
#[derive(Debug)]
pub(crate) struct DurableSendAcceptance {
    /// Append seq after the durable user message (`append_n` for the next
    /// append).
    pub append_seq: u64,
    /// The validated journal.append success ack.
    pub append_ack: Value,
    /// The exact `CommandResult` object the RPC was answered with.
    pub send_result: Value,
}

/// THE fake-Node `instance.send` acceptance contract, in ONE function shared
/// by the e2e harness and the accept_reply regression:
/// 1. durably `journal.append` the command's user message;
/// 2. await the MATCHING append ack — an RPC error returns `Err` and NO send
///    reply is written (the caller maps it to the send RPC error);
/// 3. answer the Hub RPC with the REAL typed `CommandResult`.
///
/// `seq_hint` is `None` for the harness and the accepting cases; the
/// rejected-append regression pins a hint past the durable cursor so step 2
/// deterministically fails over the SAME code. The real DevNode accepts at
/// enqueue time and the turn journals afterwards, so the harness calls this
/// BEFORE appending assistant/status events.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn accept_instance_send(
    ws: &mut NodeWs,
    queue: &mut FrameQueue,
    rpc_id: Value,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: &str,
    seq_hint: Option<i64>,
    barrier: &AckBarrier,
) -> Result<DurableSendAcceptance> {
    let (append_seq, append_ack) = durable_command_append(
        ws,
        queue,
        instance_id,
        n,
        text,
        Some(command_id),
        seq_hint,
        barrier,
    )
    .await?;
    let send_result = send_command_result_reply(ws, rpc_id, command_id, instance_id).await?;
    Ok(DurableSendAcceptance {
        append_seq,
        append_ack,
        send_result,
    })
}

/// Non-acceptance `instance.send` result shapes kept ONLY as predicate
/// regression cases in accept_reply.rs. The real acceptance path is
/// [`accept_instance_send`] and never uses these.
#[derive(Clone, Copy)]
pub(crate) enum InstanceSendReply {
    /// `{accepted:true}` at the result top level (no command object).
    TopLevelAccepted,
    /// The pre-fix fake-node reply; never a durable acceptance.
    BareOk,
}

/// Answer one `instance.send` RPC with an alternative shape (after the shared
/// durable append already succeeded); returns the exact result object sent.
pub(crate) async fn send_instance_send_rpc(
    ws: &mut NodeWs,
    id: Value,
    shape: InstanceSendReply,
) -> Result<Value> {
    let result = match shape {
        InstanceSendReply::TopLevelAccepted => json!({ "accepted": true }),
        InstanceSendReply::BareOk => json!({ "ok": true }),
    };
    send_rpc_ok(ws, id, result.clone()).await?;
    Ok(result)
}
