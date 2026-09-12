//! Node JSON-RPC socket and browser follow multiplexer.

use crate::AppState;
use crate::auth::{hash_secret, presented_token, require_device, require_origin, verify_secret};
use crate::config::now_rfc3339;
use crate::error::{HubError, rpc_error as rpc_err, rpc_ok};
use crate::inventory;
use crate::store::{HostAuthOutcome, HostAuthRequest, HostRecord, JournalRecord};
use crate::transport::{TransportKind, WssTransport};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use remuda_protocol::hubnode::{
    self, HubNodeMethod, JournalAppendParams, NodeHelloParams, TtyFrameParams,
};
use remuda_protocol::{
    ConnectionLease, HeartbeatResult, HelloResult, PROTOCOL_VERSION, TransportLimits, U64,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

/// Live event for `/v1/follow`.
#[derive(Clone, Debug)]
pub struct FollowEvent {
    /// Instance journal.
    pub instance_id: String,
    /// Seq.
    pub seq: i64,
    /// Event JSON.
    pub event: Value,
}

/// Broadcast bus for follow sockets.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<FollowEvent>,
}

impl Bus {
    /// Create a bounded broadcast channel.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    /// Publish a mirrored journal event.
    pub fn publish(&self, event: FollowEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe to live events.
    pub fn subscribe(&self) -> broadcast::Receiver<FollowEvent> {
        self.tx.subscribe()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

/// WS + follow routes. Placement/fleet merge on later.
pub fn routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/v1/follow", axum::routing::get(follow_socket))
        .route("/v1/node", axum::routing::get(node_socket))
        .route("/node/v1/connect", axum::routing::get(node_socket))
}

#[derive(Deserialize)]
pub struct FollowQuery {
    #[serde(rename = "instanceId")]
    instance_id: Option<String>,
}

/// `GET /v1/node` (and `/node/v1/connect`).
pub async fn node_socket(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    let token = presented_token(&headers).ok_or(HubError::Unauthenticated)?;
    Ok(ws.on_upgrade(move |socket| node_session(state, socket, token)))
}

/// `GET /v1/follow`
pub async fn follow_socket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FollowQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, HubError> {
    require_origin(&headers, &state.config)?;
    let device = require_device(&state.store, &headers).await?;
    let filter = query.instance_id;
    Ok(ws.on_upgrade(move |socket| follow_session(state, socket, filter, device.id)))
}

async fn node_session(state: AppState, socket: WebSocket, token: String) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(32);
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut host_id: Option<String> = None;
    let mut hello_done = false;

    loop {
        tokio::select! {
            outgoing = out_rx.recv() => {
                let Some(value) = outgoing else { break; };
                if sink.send(Message::Text(value.to_string().into())).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => {
                let Some(Ok(msg)) = incoming else { break; };
                if let Message::Binary(bytes) = &msg {
                    if hello_done {
                        handle_tty_binary(&state, host_id.as_deref(), bytes);
                    }
                    continue;
                }
                let Message::Text(text) = msg else { continue; };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue; };
                if frame.get("method").is_none() {
                    if let Some(id) = frame.get("id").and_then(Value::as_str)
                        && let Some(waiter) = pending.lock().await.remove(id)
                    {
                        let _ = waiter.send(frame);
                    }
                    continue;
                }
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                let params = frame.get("params").cloned().unwrap_or(json!({}));
                let kind = HubNodeMethod::parse(method);
                let is_hello = kind.is_some_and(HubNodeMethod::is_hello);
                let is_auth = kind.is_some_and(HubNodeMethod::is_auth);
                if !hello_done && !is_hello && !is_auth {
                    if !id.is_null() {
                        let _ = sink.send(Message::Text(
                            rpc_err(id, -32000, "runtime.hello required").to_string().into(),
                        )).await;
                    }
                    break;
                }
                match handle_node_method(&state, &token, &mut host_id, &mut hello_done, method, params, &out_tx, &pending).await {
                    Ok(Some(result)) => {
                        if !id.is_null() {
                            let _ = sink.send(Message::Text(rpc_ok(id, result).to_string().into())).await;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if !id.is_null() {
                            let _ = sink.send(Message::Text(
                                rpc_err(id, rpc_code(&err), &err.to_string()).to_string().into(),
                            )).await;
                        }
                        if matches!(err, HubError::Unauthenticated) {
                            break;
                        }
                    }
                }
            }
        }
    }

    if let Some(host_id) = host_id {
        state.nodes.remove(&host_id).await;
        let _ = state.store.mark_host_offline(host_id).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_node_method(
    state: &AppState,
    token: &str,
    host_id: &mut Option<String>,
    hello_done: &mut bool,
    method: &str,
    params: Value,
    out_tx: &mpsc::Sender<Value>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
) -> Result<Option<Value>, HubError> {
    match method {
        "node.auth" => Ok(Some(json!({
            "ok": true,
            "scheme": hubnode::AUTH_SCHEME_BEARER,
        }))),
        "runtime.hello" | "node.hello" => {
            let typed: Option<NodeHelloParams> = serde_json::from_value(params.clone()).ok();
            let hello_host = typed
                .as_ref()
                .and_then(|p| p.persisted_host_id().map(str::to_string))
                .or_else(|| {
                    params
                        .get("hostId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let label = typed.as_ref().and_then(|p| p.label.clone()).or_else(|| {
                params
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            let node_version = typed
                .as_ref()
                .and_then(|p| p.node_version.clone())
                .or_else(|| {
                    params
                        .get("nodeVersion")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let outcome = state
                .store
                .authenticate_host(
                    HostAuthRequest {
                        presented: token.to_string(),
                        bootstrap: state.config.bootstrap_token.clone(),
                        hello_host_id: hello_host,
                        label,
                        node_version,
                    },
                    verify_secret,
                    |secret| {
                        hash_secret(secret)
                            .map_err(|err| crate::store::StoreError::Id(err.to_string()))
                    },
                )
                .await?;
            let HostAuthOutcome::Authenticated { host, node_token } = outcome else {
                return Err(HubError::Unauthenticated);
            };
            *host_id = Some(host.host_id.clone());
            *hello_done = true;
            let mut inventory = inventory::from_node_params(&params);
            if inventory.transport.is_none() {
                inventory.transport = Some(TransportKind::OutboundWss);
            }
            let host = state
                .store
                .apply_inventory(
                    host.host_id.clone(),
                    inventory,
                    params.get("capabilities").cloned(),
                )
                .await?;
            state
                .nodes
                .insert(
                    host.host_id.clone(),
                    Arc::new(WssTransport::new(out_tx.clone(), pending.clone())),
                )
                .await;
            Ok(Some(hello_result(&host, node_token)?))
        }
        "runtime.heartbeat" | "node.heartbeat" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            let inventory = inventory::from_node_params(&params);
            let host = state
                .store
                .apply_inventory(
                    host_id.clone(),
                    inventory,
                    params.get("capabilities").cloned(),
                )
                .await?;
            let expires = lease_expires();
            let result = HeartbeatResult {
                server_time: ts(&now_rfc3339())?,
                lease_expires_at: ts(&expires)?,
            };
            let mut value =
                serde_json::to_value(result).map_err(|err| HubError::Internal(err.to_string()))?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("hostId".into(), json!(host.host_id));
                obj.insert("state".into(), json!(host.state));
            }
            Ok(Some(value))
        }
        "host.report" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            let mut inventory = inventory::from_node_params(&params);
            if inventory.cli.is_none() {
                inventory.cli = params.get("driverInventory").cloned();
            }
            let host = state
                .store
                .apply_inventory(
                    host_id.clone(),
                    inventory,
                    params.get("capabilities").cloned(),
                )
                .await?;
            Ok(Some(json!({
                "hostRevision": "1",
                "registrySeq": "1",
                "hostId": host.host_id,
            })))
        }
        "journal.append" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            let parsed: Option<JournalAppendParams> = serde_json::from_value(params.clone()).ok();
            let instance_id = parsed
                .as_ref()
                .map(|p| p.instance_id.clone())
                .or_else(|| {
                    params
                        .get("instanceId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .ok_or_else(|| HubError::BadRequest("journal.append requires instanceId".into()))?;
            let seq = parsed
                .as_ref()
                .and_then(JournalAppendParams::seq_i64)
                .or_else(|| {
                    params.get("seq").and_then(|v| {
                        v.as_i64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                });
            let events = parsed
                .as_ref()
                .map(JournalAppendParams::events_to_append)
                .filter(|events| !events.is_empty())
                .unwrap_or_else(|| {
                    vec![
                        params
                            .get("event")
                            .cloned()
                            .unwrap_or_else(|| params.clone()),
                    ]
                });
            state
                .store
                .ensure_instance(host_id.clone(), instance_id.clone())
                .await
                .map_err(map_host_store)?;
            let mut last = None;
            for event in events {
                let record = state
                    .store
                    .append_journal(host_id.clone(), instance_id.clone(), seq, event)
                    .await
                    .map_err(map_host_store)?;
                publish_journal(&state.bus, &record);
                crate::alerts::observe(state, &record);
                last = Some(record);
            }
            let record = last.ok_or_else(|| {
                HubError::BadRequest("journal.append requires event or events".into())
            })?;
            Ok(Some(json!({
                "seq": record.seq.to_string(),
                "eventId": record.event_id,
                "durableSeq": record.seq.to_string(),
                "watermark": { "durableSeq": record.seq.to_string() },
            })))
        }
        "tty.frame" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            let typed: Option<TtyFrameParams> = serde_json::from_value(params.clone()).ok();
            let instance_id = typed
                .as_ref()
                .and_then(|p| p.instance_id.clone())
                .or_else(|| {
                    params
                        .get("instanceId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if !instance_id.is_empty() {
                let instance = state
                    .store
                    .get_instance(instance_id.clone())
                    .await?
                    .ok_or(HubError::NotFound)?;
                if instance.host_id != *host_id {
                    return Err(HubError::Forbidden);
                }
                state.bus.publish(FollowEvent {
                    instance_id,
                    seq: 0,
                    event: json!({ "type": "tty.frame", "params": params }),
                });
            }
            Ok(Some(json!({ "ok": true })))
        }
        "instance.create"
        | "instance.send"
        | "instance.cancel"
        | "instance.respond"
        | "interaction.respond" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            if let Some(command_id) = params
                .get("commandId")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                let _ = state.store.mark_settled(command_id, host_id.clone()).await;
            }
            Ok(Some(json!({ "ok": true })))
        }
        other => Err(HubError::BadRequest(format!("unknown method {other}"))),
    }
}

fn handle_tty_binary(state: &AppState, host_id: Option<&str>, bytes: &[u8]) {
    let max = default_limits().max_binary_chunk_bytes;
    let Ok((header, payload)) = hubnode::decode_tty_binary_frame(bytes, max) else {
        return;
    };
    let Some(host_id) = host_id else {
        return;
    };
    state.bus.publish(FollowEvent {
        instance_id: host_id.to_owned(),
        seq: 0,
        event: json!({
            "type": "tty.frame",
            "binary": true,
            "channel": header.channel as u8,
            "offset": header.offset,
            "payloadLength": payload.len(),
            "envelope": hubnode::TtyBinaryEnvelopeSpec::v1(),
        }),
    });
}

fn map_host_store(err: crate::store::StoreError) -> HubError {
    let message = err.to_string();
    if message.contains("another host") {
        HubError::Forbidden
    } else {
        HubError::BadRequest(message)
    }
}

fn publish_journal(bus: &Bus, record: &JournalRecord) {
    bus.publish(FollowEvent {
        instance_id: record.instance_id.clone(),
        seq: record.seq,
        event: record.event.clone(),
    });
}

fn hello_result(host: &HostRecord, node_token: Option<String>) -> Result<Value, HubError> {
    let connection_id =
        crate::config::new_id("obj").map_err(|e| HubError::Internal(e.to_string()))?;
    let server_epoch =
        crate::config::new_id("epoch").map_err(|e| HubError::Internal(e.to_string()))?;
    let lease_id = crate::config::new_id("obj").map_err(|e| HubError::Internal(e.to_string()))?;
    let expires = lease_expires();
    let result = HelloResult {
        protocol: PROTOCOL_VERSION,
        connection_id: remuda_protocol::Id::try_from(connection_id)
            .map_err(|e| HubError::Internal(e.to_string()))?,
        server_epoch: remuda_protocol::Id::try_from(server_epoch)
            .map_err(|e| HubError::Internal(e.to_string()))?,
        observation_schema_major: remuda_protocol::SchemaVersion,
        features: vec!["snapshot-follow-v1".into(), "tty-binary-v1".into()],
        limits: default_limits(),
        lease: ConnectionLease {
            lease_id: remuda_protocol::Id::try_from(lease_id)
                .map_err(|e| HubError::Internal(e.to_string()))?,
            fence: U64(1),
            expires_at: ts(&expires)?,
        },
        reconcile_required: false,
    };
    let mut value =
        serde_json::to_value(result).map_err(|err| HubError::Internal(err.to_string()))?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("hostId".into(), json!(host.host_id));
        if let Some(token) = node_token {
            obj.insert("nodeToken".into(), json!(token));
        }
    }
    Ok(value)
}

fn default_limits() -> TransportLimits {
    TransportLimits {
        max_json_frame_bytes: 1_048_576,
        max_binary_chunk_bytes: 65_536,
        max_tty_input_bytes: 4_096,
        max_in_flight_rpc: 32,
        max_events_per_batch: 64,
        max_subscription_buffer_events: 256,
        heartbeat_interval_ms: 15_000,
        lease_ttl_ms: 60_000,
        max_wait_ms: 30_000,
    }
}

fn lease_expires() -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(60);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

fn ts(value: &str) -> Result<remuda_protocol::Timestamp, HubError> {
    remuda_protocol::Timestamp::try_from(value.to_string())
        .map_err(|err| HubError::Internal(err.to_string()))
}

fn rpc_code(err: &HubError) -> i32 {
    match err {
        HubError::Unauthenticated => -32000,
        HubError::Forbidden => -32001,
        HubError::NotFound => -32601,
        HubError::BadRequest(_) => -32602,
        _ => -32603,
    }
}

async fn follow_session(
    state: AppState,
    socket: WebSocket,
    filter: Option<String>,
    device_id: String,
) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = state.bus.subscribe();
    let mut instance_ids: Vec<String> = filter.into_iter().collect();
    for id in &instance_ids {
        state.followers.watch(device_id.clone(), id.clone()).await;
    }

    if let Some(id) = instance_ids.first().cloned() {
        let _ = send_snapshot(&state, &mut sink, &id).await;
    }

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(msg)) = incoming else { break; };
                let Message::Text(text) = msg else { continue; };
                if let Ok(value) = serde_json::from_str::<Value>(&text)
                    && value.get("type").and_then(Value::as_str) == Some("subscribe")
                    && let Some(ids) = value.get("instanceIds").and_then(Value::as_array)
                {
                    state.followers.unwatch(&device_id, &instance_ids).await;
                    instance_ids = ids
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    for id in &instance_ids {
                        state.followers.watch(device_id.clone(), id.clone()).await;
                        let _ = send_snapshot(&state, &mut sink, id).await;
                    }
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        if !instance_ids.is_empty()
                            && !instance_ids.iter().any(|id| id == &event.instance_id)
                        {
                            continue;
                        }
                        let frame = json!({
                            "type": "event",
                            "instanceId": event.instance_id,
                            "seq": event.seq.to_string(),
                            "event": event.event,
                        });
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    state.followers.unwatch(&device_id, &instance_ids).await;
}

async fn send_snapshot(
    state: &AppState,
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    instance_id: &str,
) -> Result<(), HubError> {
    let (events, durable) = state.store.read_journal(instance_id.to_string(), 0).await?;
    let frame = json!({
        "type": "snapshot",
        "instanceId": instance_id,
        "asOfSeq": durable.to_string(),
        "events": events,
    });
    sink.send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|err| HubError::Internal(err.to_string()))?;
    Ok(())
}
