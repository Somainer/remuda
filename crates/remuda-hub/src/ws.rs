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
    self, HubNodeMethod, JournalAppendParams, METHOD_OBJECT_CHUNK, NodeHelloParams, TtyFrameParams,
    TtyModeParams,
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

/// Cap on remembered tty stream bindings across all hosts.
const MAX_TTY_STREAMS: usize = 1_024;

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

/// Stream UUID → owning host/instance, plus a bounded output cache for
/// snapshot-on-attach.
///
/// Bindings are keyed by *host*, not by socket. A Node reconnect replaces the
/// socket but keeps the same host id, so a follow socket opened before the
/// reconnect keeps receiving output once the Node re-announces its streams —
/// previously the per-socket ownership set was empty on the new socket and
/// every binary frame was dropped silently while the snapshot still painted
/// (B2).
#[derive(Clone, Default)]
pub struct TtyRelay {
    streams: Arc<std::sync::Mutex<HashMap<String, StreamOwner>>>,
    buffers: Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>,
}

/// Host-validated owner of a tty stream UUID.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StreamOwner {
    host_id: String,
    instance_id: String,
}

impl TtyRelay {
    fn bind(&self, stream_uuid: &str, host_id: &str, instance_id: &str) {
        if let Ok(mut streams) = self.streams.lock() {
            if streams.len() >= MAX_TTY_STREAMS && !streams.contains_key(stream_uuid) {
                // A Node with more live streams than this recycles the map
                // rather than growing it without bound.
                streams.clear();
            }
            // An instance has one active terminal stream. Retire its old
            // identity so delayed mode notices cannot overwrite the state of
            // a replacement stream after reopen or recovery.
            streams.retain(|uuid, owner| {
                uuid == stream_uuid || owner.host_id != host_id || owner.instance_id != instance_id
            });
            streams.insert(
                stream_uuid.to_owned(),
                StreamOwner {
                    host_id: host_id.to_owned(),
                    instance_id: instance_id.to_owned(),
                },
            );
        }
    }

    /// Instance behind a stream UUID, only when `host_id` is the binder.
    ///
    /// The registry is process-wide, so without the host check a Node could
    /// push binary frames onto a stream a different Node registered (T2).
    fn instance_for_host(&self, stream_uuid: &str, host_id: &str) -> Option<String> {
        self.streams.lock().ok().and_then(|streams| {
            streams
                .get(stream_uuid)
                .filter(|owner| owner.host_id == host_id)
                .map(|owner| owner.instance_id.clone())
        })
    }

    fn instance_of(&self, stream_uuid: &str) -> Option<String> {
        self.streams.lock().ok().and_then(|streams| {
            streams
                .get(stream_uuid)
                .map(|owner| owner.instance_id.clone())
        })
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
    if crate::agent_scope::origin(&device) == remuda_protocol::InputOrigin::Agent {
        return Err(HubError::Forbidden);
    }
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
                    if let Some(host_id) = host_id.as_deref().filter(|_| hello_done) {
                        handle_tty_binary(&state, host_id, bytes);
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
                    // Terminal: write straight to the socket so the frame is
                    // flushed before this loop drops the connection.
                    if !id.is_null() {
                        let _ = sink
                            .send(Message::Text(
                                rpc_err(id, -32000, "runtime.hello required").to_string().into(),
                            ))
                            .await;
                    }
                    break;
                }
                match handle_node_method(&state, &token, &mut host_id, &mut hello_done, &mut session_generation, method, params, &out_tx, &pending).await {
                    // Normal replies ride the same FIFO as handler-emitted
                    // frames (object.pull streams its chunks just before this
                    // reply), so they are queued rather than written straight
                    // to the socket — a direct send could overtake queued
                    // chunks.
                    Ok(Some(result)) => {
                        if !id.is_null() {
                            let _ = out_tx.send(rpc_ok(id, result)).await;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        // Unauthenticated is terminal: flush the error directly
                        // before closing (a queued frame could be dropped).
                        if !id.is_null() {
                            let _ = sink
                                .send(Message::Text(
                                    rpc_err(id, rpc_code(&err), &err.to_string())
                                        .to_string()
                                        .into(),
                                ))
                                .await;
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
pub(crate) async fn handle_node_method(
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
            crate::workspaces::observe_inventory(state, &host.host_id, &params).await?;
            if params["daemon"] == true {
                state
                    .store
                    .reconcile_daemon_instances(
                        host.host_id.clone(),
                        params["instances"].as_array().cloned().unwrap_or_default(),
                    )
                    .await?;
            }
            reconcile_lost_instances(state, &host.host_id, &params).await?;
            let generation = state
                .nodes
                .insert(
                    host.host_id.clone(),
                    Arc::new(
                        WssTransport::new(out_tx.clone(), pending.clone()).with_kind(
                            TransportKind::parse(&host.transport)
                                .unwrap_or(TransportKind::OutboundWss),
                        ),
                    ),
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
            crate::workspaces::observe_inventory(state, &host.host_id, &params).await?;
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
            crate::workspaces::observe_inventory(state, &host.host_id, &params).await?;
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
            let mut next_seq = seq;
            // Fold the frame into bounded writer jobs: one transaction per
            // chunk, awaited before the next so the writer yields between
            // batches instead of holding its single connection for a whole
            // 256-event replay page (hub-store-1).
            for chunk in events.chunks(crate::store::APPEND_CHUNK_MAX) {
                let appended_chunk = state
                    .store
                    .append_journal_batch(
                        host_id.clone(),
                        instance_id.clone(),
                        next_seq,
                        chunk.to_vec(),
                    )
                    .await
                    .map_err(map_host_store)?;
                for appended in appended_chunk {
                    if !appended.replayed {
                        publish_journal(&state.bus, &appended.record);
                        crate::alerts::observe(state, &appended.record);
                        crate::usage_store::observe_journal(state, &appended.record).await;
                        crate::supply::observe_journal_text(state, &appended.record).await;
                    }
                    next_seq = Some(appended.record.seq.saturating_add(1));
                    last = Some(appended);
                }
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
        "tty.mode" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            let mode: TtyModeParams = serde_json::from_value(params)
                .map_err(|error| HubError::BadRequest(format!("tty.mode: {error}")))?;
            let instance_id = mode.instance_id.as_str();
            let stream_id = mode.stream_id.as_str();
            let uuid = stream_uuid_of(stream_id)
                .ok_or_else(|| HubError::BadRequest("tty.mode has invalid streamId".into()))?;
            let instance = state
                .store
                .get_instance(instance_id.to_owned())
                .await?
                .ok_or(HubError::NotFound)?;
            if instance.host_id != *host_id
                || state.tty.instance_for_host(&uuid, host_id).as_deref() != Some(instance_id)
            {
                return Err(HubError::Forbidden);
            }
            state.bus.publish(FollowEvent::json(
                instance_id,
                0,
                json!({ "type": "tty.mode", "params": mode }),
            ));
            Ok(Some(json!({ "ok": true })))
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
                    // Ownership is recorded against the host, not this
                    // socket, so the binding survives a Node reconnect and a
                    // different Node still cannot push onto this stream (T2).
                    state.tty.bind(&uuid, host_id, &instance_id);
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
        "gate.event" => {
            // Batch 6 co-lanes: streamed lane-runner events. The frame rides
            // the Node's authenticated session; the queue re-checks that the
            // job's lane host matches.
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            crate::gatequeue::on_event(state, host_id.as_str(), params).await?;
            Ok(Some(json!({ "ok": true })))
        }
        "instance.create"
        | "instance.send"
        | "instance.configure"
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
        "object.pull" => {
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            Ok(Some(object_pull(state, host_id, &params, out_tx).await?))
        }
        other => Err(HubError::BadRequest(format!("unknown method {other}"))),
    }
}

/// Handle a Node's `object.pull` over the node socket (D-027 carrier path).
///
/// Authorized by the host token already authenticated on this socket, and
/// additionally bound to the staging instance named in the request: the object
/// must be staged for *an instance on this host* or the pull is refused, so a
/// host token can never read another host's attachments.
///
/// Objects whose base64 form fits one frame return `dataBase64` inline; larger
/// objects are streamed as `object.chunk` notifications (FIFO on the same
/// outbound channel, so they cannot overtake this metadata-only reply) and are
/// capped at the Hub's configured attachment maximum.
async fn object_pull(
    state: &AppState,
    authenticated_host: &str,
    params: &Value,
    out_tx: &mpsc::Sender<Value>,
) -> Result<Value, HubError> {
    let object_id = params
        .get("objectId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HubError::BadRequest("object.pull requires objectId".into()))?;
    let instance_id = params
        .get("instanceId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HubError::BadRequest("object.pull requires instanceId".into()))?;
    let object = state
        .store
        .get_object(object_id.to_owned())
        .await?
        .ok_or(HubError::NotFound)?;
    // Same lazy-expiry rule as the HTTP download path.
    if crate::config::now_rfc3339().as_str() >= object.expires_at.as_str() {
        return Err(HubError::NotFound);
    }
    if object.host_id != authenticated_host || object.instance_id != instance_id {
        // Do not distinguish "wrong host" from "wrong instance / unknown
        // object" beyond the status class: both are cross-scope reads.
        return Err(HubError::Forbidden);
    }
    if object.byte_len.max(0) as usize > state.config.attachment_max_bytes {
        return Err(HubError::BadRequest(format!(
            "RESOURCE_LIMIT: attachment is {} bytes; the limit is {}",
            object.byte_len, state.config.attachment_max_bytes
        )));
    }
    let bytes = state
        .store
        .read_object_bytes(object_id.to_owned())
        .await?
        .ok_or(HubError::NotFound)?;
    let metadata = json!({
        "objectId": object.object_id,
        "mime": object.media_type,
        "size": bytes.len(),
        "sha256": object.digest,
    });
    let encoded = encode_b64(&bytes);
    if encoded.len() <= crate::objects::MAX_INLINE_PULL_BASE64 {
        let mut result = metadata;
        result["dataBase64"] = Value::String(encoded);
        return Ok(result);
    }
    // Streamed transfer. Each chunk is its own JSON frame; yield after sending
    // so control/tty traffic sharing this session is not starved while tens of
    // megabytes queue ahead of it.
    let chunks = bytes.chunks(crate::objects::PULL_CHUNK_BYTES).count();
    for (seq, chunk) in bytes.chunks(crate::objects::PULL_CHUNK_BYTES).enumerate() {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": METHOD_OBJECT_CHUNK,
            "params": {
                "objectId": object_id,
                "seq": seq as u32,
                "last": seq + 1 == chunks,
                "dataBase64": encode_b64(chunk),
            },
        });
        if out_tx.send(frame).await.is_err() {
            return Err(HubError::Internal(
                "node session closed during object.pull".into(),
            ));
        }
        tokio::task::yield_now().await;
    }
    Ok(metadata)
}

/// Reconcile Hub-side instance rows against what the Node reports at hello.
///
/// A Node restart loses every in-memory instance: the Hub keeps rows in
/// `running`/`requested` that no epoch will ever settle, they hold placement
/// slots, and a later stop never completes. When the announced `nodeEpoch`
/// differs from the one recorded for this host, every live row the Node no
/// longer lists is projected to `exited` with a Hub-authored diagnostic so the
/// loss is visible in the journal instead of silent.
///
/// Nodes that omit `instances` (a plain, non-daemon hello) report nothing, so
/// reconciliation only runs when an epoch change is actually observed and the
/// Node did send an inventory — otherwise a stateless Node would wipe rows it
/// simply never enumerates.
async fn reconcile_lost_instances(
    state: &AppState,
    host_id: &str,
    params: &Value,
) -> Result<(), HubError> {
    let epoch = params
        .get("nodeEpoch")
        .and_then(Value::as_str)
        .map(str::to_string);
    let changed = state
        .store
        .record_node_epoch(host_id.to_string(), epoch)
        .await?;
    if !changed {
        return Ok(());
    }
    let Some(reported) = params.get("instances").and_then(Value::as_array) else {
        tracing::warn!(
            %host_id,
            "node epoch changed but hello carried no instance inventory; \
             leaving instance rows untouched"
        );
        return Ok(());
    };
    let reported: Vec<String> = reported
        .iter()
        .filter_map(|item| {
            item.get("id")
                .or_else(|| item.get("instanceId"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let lost = state
        .store
        .reconcile_reported_instances(
            host_id.to_string(),
            reported,
            "node-epoch-changed".to_string(),
        )
        .await?;
    for instance_id in lost {
        tracing::warn!(
            %host_id,
            %instance_id,
            "node epoch changed; instance lost"
        );
        publish_hub_diagnostic(
            state,
            &instance_id,
            "node_epoch_changed",
            "node epoch changed; instance lost",
        )
        .await;
    }
    Ok(())
}

/// Append and fan out a Hub-authored diagnostic, logging rather than failing.
pub(crate) async fn publish_hub_diagnostic(
    state: &AppState,
    instance_id: &str,
    native_name: &str,
    message: &str,
) {
    match state
        .store
        .append_hub_diagnostic(
            instance_id.to_string(),
            native_name.to_string(),
            message.to_string(),
        )
        .await
    {
        Ok(Some(record)) => publish_journal(&state.bus, &record),
        Ok(None) => {}
        Err(error) => tracing::error!(
            %instance_id,
            %error,
            "failed to journal hub diagnostic"
        ),
    }
}

/// Strip the `tty_` registry prefix from a stream ID, keeping the bare UUID.
///
/// Binary headers carry the UUID without the prefix, so both sides of the
/// stream registry are keyed on the canonical UUID text.
/// Publish a binary tty frame to the instance that registered its stream.
///
/// The frame header carries only a stream UUID — no instance or host id — so
/// attribution comes from the stream registry, never from the sending host.
/// The lookup is additionally scoped to the streams *this host* bound, because
/// [`TtyRelay`] is process-wide: without it a Node could push binary frames
/// onto a stream a different Node registered. An unregistered stream is
/// dropped rather than published under a guessed id. Scoping by host rather
/// than by socket is what lets output resume after a Node reconnect (B2).
fn handle_tty_binary(state: &AppState, host_id: &str, bytes: &[u8]) {
    let max = default_limits().max_binary_chunk_bytes;
    let Ok((header, payload)) = hubnode::decode_tty_binary_frame(bytes, max) else {
        return;
    };
    if header.channel != remuda_protocol::BinaryChannel::TtyOutput {
        return;
    }
    let uuid = header.stream_uuid.uuid().to_string();
    // Only streams this host registered: `TtyRelay` is shared across Nodes.
    let Some(instance_id) = state.tty.instance_for_host(&uuid, host_id) else {
        return;
    };
    state.tty.push_output(&instance_id, payload);
    state.bus.publish(FollowEvent {
        instance_id,
        seq: 0,
        event: json!({
            "type": "tty.frame",
            "binary": true,
            "streamId": uuid,
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
                                handle_follow_input(&state, &out_tx, &instance_ids, &bytes).await;
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
                                handle_follow_resize(&state, &out_tx, &instance_ids, &value).await;
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
                            if !want_tty && event.event["type"] == "tty.mode" {
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

/// Replay the TTY snapshot for one instance, preferring a live `tty.attach`.
///
/// Re-attaching also re-binds the stream UUID for the owning host, which is how
/// a follow socket that outlived a Node reconnect gets its stream back (B2).
/// When the Node cannot be reached, the Hub's own cached output is sent instead
/// of being computed and dropped (B3) — the follower then paints the last known
/// screen rather than an empty terminal.
async fn send_tty_snapshot(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_id: &str,
) -> Result<(), ()> {
    let host_id = state
        .store
        .get_instance(instance_id.to_string())
        .await
        .ok()
        .flatten()
        .map(|instance| instance.host_id);
    let mut cached_stream_id = None;
    if let Some(host_id) = host_id.as_deref() {
        match state
            .nodes
            .call(
                host_id,
                "tty.attach",
                json!({ "instanceId": instance_id, "mode": "write" }),
                Duration::from_secs(5),
            )
            .await
        {
            Ok(Some(response)) => {
                let result = response.get("result").cloned().unwrap_or(response);
                let stream_id = result
                    .get("streamId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(stream_id) = stream_id.as_deref()
                    && let Some(uuid) = stream_uuid_of(stream_id)
                {
                    state.tty.bind(&uuid, host_id, instance_id);
                }
                cached_stream_id = stream_id;
                // Every fresh attach supplies an authoritative mode boundary.
                // Null means this snapshot has no emulator-backed observation
                // (or an older Node omitted it); it clears a prior true/false
                // rather than making stale mode evidence look current.
                // Additive OSC 9;4 state for the header bar; an explicit null
                // clears a stale bar, an older Node that omits the key leaves
                // it as is.
                let mut notice = json!({
                    "type": "tty.mode",
                    "instanceId": instance_id,
                    "streamId": cached_stream_id,
                    "altScreen": result.get("altScreen").and_then(Value::as_bool),
                });
                if let Some(progress) = result.get("progress") {
                    notice["progress"] = progress.clone();
                }
                out_tx
                    .send(FollowMsg::Text(notice.to_string()))
                    .await
                    .map_err(|_| ())?;
                if let Some(b64) = result.get("snapshotBase64").and_then(Value::as_str)
                    && let Ok(bytes) = decode_b64(b64)
                    && !bytes.is_empty()
                {
                    state.tty.push_output(instance_id, &bytes);
                    let offset = result
                        .get("availableFrom")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    if let Some(frame) = encode_output_frame(
                        cached_stream_id.as_deref().unwrap_or(""),
                        offset,
                        &bytes,
                    ) {
                        out_tx
                            .send(FollowMsg::Binary(frame))
                            .await
                            .map_err(|_| ())?;
                        return Ok(());
                    }
                }
            }
            Ok(None) => tracing::debug!(
                %instance_id,
                %host_id,
                "tty.attach skipped: node offline; falling back to the cached snapshot"
            ),
            Err(error) => tracing::warn!(
                %instance_id,
                %host_id,
                %error,
                "tty.attach failed; falling back to the cached snapshot"
            ),
        }
    }
    // B3: the Hub's own bounded cache is the last resort. Without a stream id
    // from the Node there is nothing to address a binary frame to, so the
    // cached bytes travel as a JSON diagnostic the follower can still render.
    let cached = state.tty.snapshot(instance_id);
    if cached.is_empty() {
        return Ok(());
    }
    let frame = cached_stream_id
        .as_deref()
        .and_then(|stream_id| encode_output_frame(stream_id, 0, &cached));
    let message = match frame {
        Some(frame) => FollowMsg::Binary(frame),
        None => FollowMsg::Text(
            json!({
                "type": "tty.snapshot",
                "instanceId": instance_id,
                "source": "hub-cache",
                "dataBase64": encode_b64(&cached),
            })
            .to_string(),
        ),
    };
    out_tx.send(message).await.map_err(|_| ())?;
    Ok(())
}

/// Tell the follower that one of its tty operations did not reach the PTY.
///
/// Every input failure used to be a bare `return`, so a dead keyboard looked
/// exactly like an idle terminal from both the browser and the Hub log (B4).
async fn send_tty_diagnostic(
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_id: Option<&str>,
    operation: &str,
    reason: &str,
    detail: &str,
) {
    let frame = json!({
        "type": "tty.diagnostic",
        "instanceId": instance_id,
        "operation": operation,
        "reason": reason,
        "detail": detail,
    });
    // A closed follow socket is the caller's next read; nothing to report here.
    let _ = out_tx.try_send(FollowMsg::Text(frame.to_string()));
}

async fn handle_follow_input(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_ids: &[String],
    bytes: &[u8],
) {
    let max = default_limits().max_tty_input_bytes;
    let first = instance_ids.first().map(String::as_str);
    let (header, payload) = match hubnode::decode_tty_binary_frame(bytes, max) {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!(
                instance_id = ?first,
                frame_bytes = bytes.len(),
                %error,
                "follow tty input frame rejected"
            );
            send_tty_diagnostic(
                out_tx,
                first,
                "tty.write",
                "malformed-frame",
                &error.to_string(),
            )
            .await;
            return;
        }
    };
    if header.channel != remuda_protocol::BinaryChannel::TtyInput {
        tracing::warn!(
            instance_id = ?first,
            channel = header.channel as u8,
            "follow tty input frame on the wrong channel"
        );
        send_tty_diagnostic(
            out_tx,
            first,
            "tty.write",
            "wrong-channel",
            "tty input must use the tty-input binary channel",
        )
        .await;
        return;
    }
    if payload.len() > max as usize {
        tracing::warn!(
            instance_id = ?first,
            payload_bytes = payload.len(),
            max,
            "follow tty input exceeds maxTtyInputBytes"
        );
        send_tty_diagnostic(
            out_tx,
            first,
            "tty.write",
            "payload-too-large",
            &format!("{} bytes exceeds maxTtyInputBytes {max}", payload.len()),
        )
        .await;
        return;
    }
    let uuid = header.stream_uuid.uuid().to_string();
    let instance_id = state
        .tty
        .instance_of(&uuid)
        .or_else(|| instance_ids.first().cloned());
    let Some(instance_id) = instance_id else {
        tracing::warn!(
            stream_uuid = %uuid,
            "follow tty input has no bound stream and no subscribed instance"
        );
        send_tty_diagnostic(
            out_tx,
            None,
            "tty.write",
            "unbound-stream",
            "no instance is bound to this tty stream",
        )
        .await;
        return;
    };
    forward_tty_call(
        state,
        out_tx,
        &instance_id,
        "tty.write",
        json!({
            "instanceId": instance_id,
            "dataBase64": encode_b64(payload),
        }),
    )
    .await;
}

async fn handle_follow_resize(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_ids: &[String],
    value: &Value,
) {
    let Some(instance_id) = instance_ids.first() else {
        tracing::warn!("follow tty.resize arrived before any instance subscription");
        send_tty_diagnostic(
            out_tx,
            None,
            "tty.resize",
            "no-subscription",
            "subscribe to an instance before resizing",
        )
        .await;
        return;
    };
    let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(80);
    let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(24);
    forward_tty_call(
        state,
        out_tx,
        instance_id,
        "tty.resize",
        json!({ "instanceId": instance_id, "cols": cols, "rows": rows }),
    )
    .await;
}

/// Relay one tty RPC to the owning Node, logging and reporting every failure.
///
/// Node errors used to be swallowed by `let _ =`, which is why an input that
/// never reached the PTY produced no log line and no client-visible signal.
async fn forward_tty_call(
    state: &AppState,
    out_tx: &mpsc::Sender<FollowMsg>,
    instance_id: &str,
    method: &str,
    params: Value,
) {
    let host_id = match state.store.get_instance(instance_id.to_string()).await {
        Ok(Some(instance)) => instance.host_id,
        Ok(None) => {
            tracing::warn!(%instance_id, %method, "tty relay target instance is unknown");
            send_tty_diagnostic(
                out_tx,
                Some(instance_id),
                method,
                "unknown-instance",
                "the Hub has no record of this instance",
            )
            .await;
            return;
        }
        Err(error) => {
            tracing::error!(%instance_id, %method, %error, "tty relay instance lookup failed");
            send_tty_diagnostic(
                out_tx,
                Some(instance_id),
                method,
                "lookup-failed",
                &error.to_string(),
            )
            .await;
            return;
        }
    };
    match state
        .nodes
        .call(&host_id, method, params, Duration::from_secs(5))
        .await
    {
        Ok(Some(response)) if response.get("error").is_some() => {
            let detail = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("node rejected the tty call")
                .to_string();
            tracing::warn!(%instance_id, %host_id, %method, %detail, "node rejected a tty call");
            send_tty_diagnostic(out_tx, Some(instance_id), method, "node-error", &detail).await;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(%instance_id, %host_id, %method, "tty call dropped: node offline");
            send_tty_diagnostic(
                out_tx,
                Some(instance_id),
                method,
                "node-offline",
                "no live Node session for this host",
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(%instance_id, %host_id, %method, %error, "tty call failed");
            send_tty_diagnostic(
                out_tx,
                Some(instance_id),
                method,
                "call-failed",
                &error.to_string(),
            )
            .await;
        }
    }
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

    #[test]
    fn replacement_terminal_stream_retires_stale_mode_source() {
        let relay = TtyRelay::default();
        relay.bind("old-stream", "host-a", "instance-a");
        relay.bind("other-stream", "host-a", "instance-b");
        relay.bind("new-stream", "host-a", "instance-a");
        assert_eq!(relay.instance_for_host("old-stream", "host-a"), None);
        assert_eq!(relay.instance_for_host("new-stream", "host-b"), None);
        assert_eq!(
            relay.instance_for_host("new-stream", "host-a").as_deref(),
            Some("instance-a")
        );
        assert_eq!(
            relay.instance_for_host("other-stream", "host-a").as_deref(),
            Some("instance-b")
        );
    }

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
