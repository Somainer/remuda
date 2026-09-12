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
use std::time::Duration;
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
    /// Binary `tty.frame` envelope when this is TTY output.
    pub binary: Option<Vec<u8>>,
}

impl FollowEvent {
    /// Journal / JSON follow event.
    #[must_use]
    pub fn json(instance_id: impl Into<String>, seq: i64, event: Value) -> Self {
        Self {
            instance_id: instance_id.into(),
            seq,
            event,
            binary: None,
        }
    }
}

/// Stream UUID → instance and bounded output cache for snapshot-on-attach.
#[derive(Clone, Default)]
pub struct TtyRelay {
    streams: Arc<std::sync::Mutex<HashMap<String, String>>>,
    buffers: Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>,
}

impl TtyRelay {
    fn bind(&self, stream_uuid: &str, instance_id: &str) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.insert(stream_uuid.to_owned(), instance_id.to_owned());
        }
    }

    fn instance_of(&self, stream_uuid: &str) -> Option<String> {
        self.streams
            .lock()
            .ok()
            .and_then(|streams| streams.get(stream_uuid).cloned())
    }

    fn push_output(&self, instance_id: &str, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        if let Ok(mut buffers) = self.buffers.lock() {
            let buf = buffers.entry(instance_id.to_owned()).or_default();
            buf.extend_from_slice(payload);
            const MAX: usize = 256 * 1024;
            if buf.len() > MAX {
                let drop = buf.len() - MAX;
                buf.drain(..drop);
            }
        }
    }

    fn snapshot(&self, instance_id: &str) -> Vec<u8> {
        self.buffers
            .lock()
            .ok()
            .and_then(|buffers| buffers.get(instance_id).cloned())
            .unwrap_or_default()
    }
}

/// Broadcast bus for follow sockets.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<FollowEvent>,
}

impl Bus {
    /// Create a bounded broadcast channel.
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    /// Create a broadcast bus with `capacity` live slots (slow followers gap).
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
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
    /// `tty=1` attaches the instance TTY (snapshot then live binary frames).
    #[serde(default)]
    tty: Option<u8>,
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
    // Browsers send the HttpOnly device cookie with the same-origin handshake.
    // Native clients may use Authorization; URL parameters never authenticate.
    let device = require_device(&state.store, &headers).await?;
    let filter = query.instance_id;
    let tty = query.tty == Some(1);
    Ok(ws.on_upgrade(move |socket| follow_session(state, socket, filter, device.id, tty)))
}

async fn node_session(state: AppState, socket: WebSocket, token: String) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(32);
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut host_id: Option<String> = None;
    let mut hello_done = false;
    let mut session_generation: Option<u64> = None;

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
                if text.len() > default_limits().max_json_frame_bytes as usize {
                    break;
                }
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
                match handle_node_method(&state, &token, &mut host_id, &mut hello_done, &mut session_generation, method, params, &out_tx, &pending).await {
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
        let stale = match session_generation {
            Some(generation) => state.nodes.remove_generation(&host_id, generation).await,
            None => {
                state.nodes.remove(&host_id).await;
                true
            }
        };
        if stale {
            let _ = state.store.mark_host_offline(host_id).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_node_method(
    state: &AppState,
    token: &str,
    host_id: &mut Option<String>,
    hello_done: &mut bool,
    session_generation: &mut Option<u64>,
    method: &str,
    params: Value,
    out_tx: &mpsc::Sender<Value>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
) -> Result<Option<Value>, HubError> {
    match method {
        "node.auth" => {
            let presented = params.get("token").and_then(Value::as_str).unwrap_or("");
            if !crate::config::secret_eq(presented, token) {
                return Err(HubError::Unauthenticated);
            }
            Ok(Some(json!({
                "ok": true,
                "scheme": hubnode::AUTH_SCHEME_BEARER,
            })))
        }
        "runtime.hello" | "node.hello" => {
            if *hello_done {
                return Err(HubError::BadRequest("hello already completed".into()));
            }
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
            let generation = state
                .nodes
                .insert(
                    host.host_id.clone(),
                    Arc::new(WssTransport::new(out_tx.clone(), pending.clone())),
                )
                .await;
            *session_generation = Some(generation);
            let watermarks = state
                .store
                .list_instance_watermarks(host.host_id.clone())
                .await?;
            let mut result = hello_result(&host, node_token)?;
            if let Some(obj) = result.as_object_mut() {
                obj.insert("instanceWatermarks".into(), json!(watermarks));
            }
            Ok(Some(result))
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
            let mut seq = seq;
            for event in events {
                let appended = state
                    .store
                    .append_journal(host_id.clone(), instance_id.clone(), seq, event)
                    .await
                    .map_err(map_host_store)?;
                if !appended.replayed {
                    publish_journal(&state.bus, &appended.record);
                    crate::alerts::observe(state, &appended.record);
                }
                seq = Some(appended.record.seq.saturating_add(1));
                last = Some(appended);
            }
            let appended = last.ok_or_else(|| {
                HubError::BadRequest("journal.append requires event or events".into())
            })?;
            Ok(Some(json!({
                "seq": appended.record.seq.to_string(),
                "eventId": appended.record.event_id,
                "durableSeq": appended.durable_seq.to_string(),
                "replayed": appended.replayed,
                "watermark": { "durableSeq": appended.durable_seq.to_string() },
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
                if let Some(stream_id) =
                    typed
                        .as_ref()
                        .and_then(|p| p.stream_id.clone())
                        .or_else(|| {
                            params
                                .get("streamId")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                    && let Some(uuid) = stream_uuid_of(&stream_id)
                {
                    state.tty.bind(&uuid, &instance_id);
                }
                if let Some(b64) =
                    typed
                        .as_ref()
                        .and_then(|p| p.data_base64.clone())
                        .or_else(|| {
                            params
                                .get("dataBase64")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                    && let Ok(bytes) = decode_b64(&b64)
                {
                    state.tty.push_output(&instance_id, &bytes);
                    if let Some(frame) = encode_output_frame(
                        typed
                            .as_ref()
                            .and_then(|p| p.stream_id.as_deref())
                            .or_else(|| params.get("streamId").and_then(Value::as_str))
                            .unwrap_or(""),
                        0,
                        &bytes,
                    ) {
                        state.bus.publish(FollowEvent {
                            instance_id: instance_id.clone(),
                            seq: 0,
                            event: json!({ "type": "tty.frame", "params": params }),
                            binary: Some(frame),
                        });
                    }
                } else {
                    state.bus.publish(FollowEvent::json(
                        instance_id,
                        0,
                        json!({ "type": "tty.frame", "params": params }),
                    ));
                }
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
    let _ = host_id;
    if header.channel != remuda_protocol::BinaryChannel::TtyOutput {
        return;
    }
    let uuid = header.stream_uuid.uuid().to_string();
    let Some(instance_id) = state.tty.instance_of(&uuid) else {
        return;
    };
    state.tty.push_output(&instance_id, payload);
    state.bus.publish(FollowEvent {
        instance_id,
        seq: 0,
        event: json!({
            "type": "tty.frame",
            "binary": true,
            "channel": header.channel as u8,
            "offset": header.offset,
            "payloadLength": payload.len(),
        }),
        binary: Some(bytes.to_vec()),
    });
}

fn stream_uuid_of(stream_id: &str) -> Option<String> {
    remuda_protocol::StreamUuid::from_prefixed_id(stream_id)
        .ok()
        .map(|uuid| uuid.uuid().to_string())
}

fn decode_b64(raw: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(raw.as_bytes())
        .map_err(|_| ())
}

fn encode_b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn encode_output_frame(stream_id: &str, offset: u64, payload: &[u8]) -> Option<Vec<u8>> {
    let uuid = remuda_protocol::StreamUuid::from_prefixed_id(stream_id).ok()?;
    remuda_protocol::encode_binary_frame(
        remuda_protocol::BinaryChannel::TtyOutput,
        uuid,
        offset,
        payload,
    )
    .ok()
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
    bus.publish(FollowEvent::json(
        record.instance_id.clone(),
        record.seq,
        record.event.clone(),
    ));
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

enum FollowMsg {
    Text(String),
    Binary(Vec<u8>),
}

async fn follow_session(
    state: AppState,
    socket: WebSocket,
    filter: Option<String>,
    device_id: String,
    mut want_tty: bool,
) {
    let cap = state.config.follow_buffer_events.max(1);
    let (out_tx, mut out_rx) = mpsc::channel::<FollowMsg>(cap);
    let (mut sink, mut stream) = socket.split();
    let mut rx = state.bus.subscribe();
    let mut instance_ids: Vec<String> = filter.into_iter().collect();
    for id in &instance_ids {
        state.followers.watch(device_id.clone(), id.clone()).await;
    }

    let writer = async {
        while let Some(msg) = out_rx.recv().await {
            let send = match msg {
                FollowMsg::Text(text) => sink.send(Message::Text(text.into())).await,
                FollowMsg::Binary(bytes) => sink.send(Message::Binary(bytes.into())).await,
            };
            if send.is_err() {
                break;
            }
        }
    };

    let pump = async {
        for id in &instance_ids {
            if send_follow_snapshot(&state, &out_tx, id, want_tty)
                .await
                .is_err()
            {
                return;
            }
        }
        loop {
            tokio::select! {
                incoming = stream.next() => {
                    let Some(Ok(msg)) = incoming else { break; };
                    match msg {
                        Message::Binary(bytes) => {
                            if want_tty {
                                handle_follow_input(&state, &instance_ids, &bytes).await;
                            }
                        }
                        Message::Text(text) => {
                            let Ok(value) = serde_json::from_str::<Value>(&text) else { continue; };
                            let kind = value.get("type").and_then(Value::as_str);
                            if kind == Some("subscribe")
                                && let Some(ids) = value.get("instanceIds").and_then(Value::as_array)
                            {
                                want_tty = value.get("tty").and_then(Value::as_u64) == Some(1) || want_tty;
                                state.followers.unwatch(&device_id, &instance_ids).await;
                                instance_ids = ids
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect();
                                for id in &instance_ids {
                                    state.followers.watch(device_id.clone(), id.clone()).await;
                                    if send_follow_snapshot(&state, &out_tx, id, want_tty)
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            } else if kind == Some("tty.resize") && want_tty {
                                handle_follow_resize(&state, &instance_ids, &value).await;
                            }
                        }
                        _ => {}
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
                            let send = if want_tty && let Some(binary) = event.binary {
                                FollowMsg::Binary(binary)
                            } else if event.binary.is_some() {
                                continue;
                            } else {
                                FollowMsg::Text(json!({
                                    "type": "event",
                                    "instanceId": event.instance_id,
                                    "seq": event.seq.to_string(),
                                    "event": event.event,
                                }).to_string())
                            };
                            match out_tx.try_send(send) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    if resync_after_gap(&state, &out_tx, &instance_ids, want_tty).await.is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            if resync_after_gap(&state, &out_tx, &instance_ids, want_tty).await.is_err() {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    };

    tokio::select! {
        () = writer => {}
        () = pump => {}
    }
    state.followers.unwatch(&device_id, &instance_ids).await;
}

async fn send_follow_snapshot(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_id: &str,
    want_tty: bool,
) -> Result<(), ()> {
    if let Ok(frame) = snapshot_json(state, instance_id).await {
        out_tx
            .send(FollowMsg::Text(frame.to_string()))
            .await
            .map_err(|_| ())?;
    }
    if want_tty {
        send_tty_snapshot(state, out_tx, instance_id).await?;
    }
    Ok(())
}

async fn send_tty_snapshot(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_id: &str,
) -> Result<(), ()> {
    if let Some(host_id) = state
        .store
        .get_instance(instance_id.to_string())
        .await
        .ok()
        .flatten()
        .map(|instance| instance.host_id)
        && let Ok(Some(response)) = state
            .nodes
            .call(
                &host_id,
                "tty.attach",
                json!({ "instanceId": instance_id, "mode": "write" }),
                Duration::from_secs(5),
            )
            .await
    {
        let result = response.get("result").cloned().unwrap_or(response);
        if let Some(stream_id) = result.get("streamId").and_then(Value::as_str)
            && let Some(uuid) = stream_uuid_of(stream_id)
        {
            state.tty.bind(&uuid, instance_id);
        }
        if let Some(b64) = result.get("snapshotBase64").and_then(Value::as_str)
            && let Ok(bytes) = decode_b64(b64)
            && !bytes.is_empty()
        {
            state.tty.push_output(instance_id, &bytes);
            let stream_id = result.get("streamId").and_then(Value::as_str).unwrap_or("");
            let offset = result
                .get("availableFrom")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if let Some(frame) = encode_output_frame(stream_id, offset, &bytes) {
                out_tx
                    .send(FollowMsg::Binary(frame))
                    .await
                    .map_err(|_| ())?;
                return Ok(());
            }
        }
    }
    let cached = state.tty.snapshot(instance_id);
    if cached.is_empty() {
        return Ok(());
    }
    Ok(())
}

async fn handle_follow_input(state: &AppState, instance_ids: &[String], bytes: &[u8]) {
    let max = default_limits().max_tty_input_bytes;
    let Ok((header, payload)) = hubnode::decode_tty_binary_frame(bytes, max) else {
        return;
    };
    if header.channel != remuda_protocol::BinaryChannel::TtyInput {
        return;
    }
    if payload.len() > max as usize {
        return;
    }
    let uuid = header.stream_uuid.uuid().to_string();
    let instance_id = state
        .tty
        .instance_of(&uuid)
        .or_else(|| instance_ids.first().cloned());
    let Some(instance_id) = instance_id else {
        return;
    };
    let Some(host_id) = state
        .store
        .get_instance(instance_id.clone())
        .await
        .ok()
        .flatten()
        .map(|instance| instance.host_id)
    else {
        return;
    };
    let _ = state
        .nodes
        .call(
            &host_id,
            "tty.write",
            json!({
                "instanceId": instance_id,
                "dataBase64": encode_b64(payload),
            }),
            Duration::from_secs(5),
        )
        .await;
}

async fn handle_follow_resize(state: &AppState, instance_ids: &[String], value: &Value) {
    let Some(instance_id) = instance_ids.first() else {
        return;
    };
    let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(80);
    let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(24);
    let Some(host_id) = state
        .store
        .get_instance(instance_id.clone())
        .await
        .ok()
        .flatten()
        .map(|instance| instance.host_id)
    else {
        return;
    };
    let _ = state
        .nodes
        .call(
            &host_id,
            "tty.resize",
            json!({ "instanceId": instance_id, "cols": cols, "rows": rows }),
            Duration::from_secs(5),
        )
        .await;
}

async fn resync_after_gap(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_ids: &[String],
    want_tty: bool,
) -> Result<(), ()> {
    let gap = json!({ "type": "gap", "reason": "backpressure" });
    out_tx
        .send(FollowMsg::Text(gap.to_string()))
        .await
        .map_err(|_| ())?;
    for id in instance_ids {
        send_follow_snapshot(state, out_tx, id, want_tty).await?;
    }
    Ok(())
}

async fn snapshot_json(state: &AppState, instance_id: &str) -> Result<Value, HubError> {
    let (events, durable) = state.store.read_journal(instance_id.to_string(), 0).await?;
    Ok(json!({
        "type": "snapshot",
        "instanceId": instance_id,
        "asOfSeq": durable.to_string(),
        "events": events,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn follow_bus_reports_lag_when_capacity_exceeded() {
        let bus = Bus::with_capacity(1);
        let mut rx = bus.subscribe();
        bus.publish(FollowEvent::json("ins", 1, json!({"n": 1})));
        bus.publish(FollowEvent::json("ins", 2, json!({"n": 2})));
        bus.publish(FollowEvent::json("ins", 3, json!({"n": 3})));
        let first = rx.recv().await;
        match first {
            Err(broadcast::error::RecvError::Lagged(skipped)) => assert!(skipped >= 1),
            Ok(_) => {
                let second = rx.recv().await;
                assert!(
                    matches!(second, Err(broadcast::error::RecvError::Lagged(_))),
                    "{second:?}"
                );
            }
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
}
