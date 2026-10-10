//! Node JSON-RPC socket and browser follow multiplexer.
//!
//! RESILIENCE (spec only — `docs/design/hub-resilience.md`, 2026-09-22
//! c-hubresil): two gaps the doc pins here — (1) `/v1/follow` emits only
//! snapshot/event/gap frames, there is no idle heartbeat frame, so a phone
//! cannot today distinguish live from stale-on-an-open-socket (§2.6); the
//! `follow.tick` frame and the live/stale/disconnected/recovering criteria are
//! specified in §5. (2) the `node.hello` handler has no
//! `hello.retry_after` reply, so simultaneously woken Nodes cannot be asked to
//! back off; §4 specifies that frame. Neither is implemented yet.

use crate::AppState;
use crate::auth::{hash_secret, presented_token, require_device, require_origin, verify_secret};
use crate::config::now_rfc3339;
use crate::error::{HubError, rpc_error as rpc_err, rpc_ok};
use crate::inventory;
use crate::store::{HostAuthOutcome, HostAuthRequest, HostRecord, JournalRecord};
use crate::transport::{TransportKind, WssTransport, fail_all_pending, new_pending_rpcs};
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
    WorkerState,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// Cap on remembered tty stream bindings across all hosts.
const MAX_TTY_STREAMS: usize = 1_024;

/// c-cardsettle: one interaction invalidated because its instance ended.
/// Carried on the dedicated low-traffic [`AppState::settlement_bus`] (not the
/// journal bus) so a follower lagging on journal frames cannot miss it.
#[derive(Clone, Debug)]
pub struct SettlementNotice {
    /// Owning instance (used to filter per-instance followers).
    pub instance_id: String,
    /// The interaction id that was invalidated.
    pub interaction_id: String,
    /// Terminal state ("invalidated").
    pub state: String,
    /// Resolution reason ("generation-ended").
    pub reason: String,
    /// Per-Hub monotonic settlement seq (c-cardsettle r9 item 3); followers
    /// advance their delivery cursor on every notice so a multi-page lag
    /// drain skips no row regardless of timestamps or id ordering.
    pub seq: i64,
}

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
    buffers: Arc<std::sync::Mutex<HashMap<String, CachedScreen>>>,
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
            buf.bytes.extend_from_slice(payload);
            // Stamped on every append: the age the browser renders is the age
            // of the *newest* byte, which is what "this screen is frozen as of
            // T" means. A buffer that never re-stamped would age from first
            // attach and overstate how stale a slow-but-live stream is.
            buf.captured_at = Some(now_rfc3339());
            const MAX: usize = 256 * 1024;
            if buf.bytes.len() > MAX {
                let drop = buf.bytes.len() - MAX;
                buf.bytes.drain(..drop);
            }
        }
    }

    /// The cached bytes plus when the newest of them arrived.
    ///
    /// The timestamp is what lets a follower tell a live frame from a fossil:
    /// the Hub's cache is the last resort when `tty.attach` cannot be answered,
    /// and without an age the browser has no way to know the screen it is
    /// painting stopped changing hours ago (2026-09-18 demo).
    fn snapshot(&self, instance_id: &str) -> CachedScreen {
        self.buffers
            .lock()
            .ok()
            .and_then(|buffers| buffers.get(instance_id).cloned())
            .unwrap_or_default()
    }
}

/// The Hub's bounded per-instance byte cache, with the capture time.
///
/// Empty is the default: an instance the Hub has never seen output for has no
/// screen at all, which is different from a screen captured at an unknown
/// time, so `captured_at` stays `None` and callers send nothing.
#[derive(Clone, Default)]
struct CachedScreen {
    bytes: Vec<u8>,
    captured_at: Option<String>,
}

impl CachedScreen {
    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
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
    let pending = new_pending_rpcs();
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
                        && let Some(waiter) = pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(id)
                    {
                        let _ = waiter.tx.send(frame);
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
                        // Re-push api.egress contexts only after the hello
                        // reply is queued: the egress sends share this bounded
                        // FIFO, which this same task drains, so awaiting them
                        // inline would park the reply once a proxy held ~32+
                        // routed instances (D-048 B.2).
                        if is_hello
                            && let Some(h) = host_id.clone()
                        {
                            spawn_egress_reinstall(&state, h);
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

    // Drain every waiter first: calls in flight when the socket ends must not
    // sit until their timeout (and must never leak as dead map entries).
    fail_all_pending(&pending);

    if let Some(host_id) = host_id {
        // Resolve session staleness first: if a newer link for this host has
        // already registered (a reconnect that superseded this socket), its
        // teardown must not wipe the contexts the live link just installed and
        // block instances that are reachable again.
        let stale = match session_generation {
            Some(generation) => state.nodes.remove_generation(&host_id, generation).await,
            None => {
                state.nodes.remove(&host_id).await;
                true
            }
        };
        if stale {
            // D-048: tear down relay streams this link owned. A vanished proxy
            // host ends streams with via-host-offline and blocks routed
            // instances; a vanished worker host cancels the upstream legs.
            state.api_relay.on_link_lost(&state, &host_id).await;
            let _ = state.store.mark_host_offline(host_id).await;
        }
    }
}

/// Spawn the D-048 B.2 post-hello re-push of a host's api.egress contexts.
///
/// The task runs only after the `node.hello` reply has been queued: each
/// `install_egress` send shares the session's bounded (32-slot) outbound
/// FIFO, which the session task itself drains. Awaiting those sends inline
/// would fill the FIFO ahead of the reply and park the 33rd, so the hello
/// would never answer for a proxy routing ~32+ instances; below that, every
/// egress frame queued ahead of the reply. The spawned task queues after the
/// reply and is independently drained.
pub(crate) fn spawn_egress_reinstall(state: &AppState, host_id: String) {
    let state = state.clone();
    tokio::spawn(async move {
        state
            .api_relay
            .reinstall_egress_on_connect(&state, &host_id)
            .await;
    });
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
    pending: &crate::transport::PendingRpcs,
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
                    .publish_settlement_unit(state.store.reconcile_daemon_instances(
                        host.host_id.clone(),
                        params["instances"].as_array().cloned().unwrap_or_default(),
                    ))
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
            // Set when this frame carries a *fresh* terminal lifecycle event
            // for the instance — the normal-exit path must revoke the egress
            // context from H exactly once, after the batch is durable.
            let mut instance_terminated = false;
            // Fold the frame into bounded writer jobs: one transaction per
            // chunk, awaited before the next so the writer yields between
            // batches instead of holding its single connection for a whole
            // 256-event replay page. A chunk is bounded by event count AND by
            // serialized bytes, so a frame of large transcript/screen events
            // cannot smuggle one long transaction past the count cap
            // (hub-store-1).
            for range in crate::store::journal_append_chunks(&events) {
                // r8 item 4 / r9 item 4(c): the publication lock covers ONLY
                // the settling commit and the settlement-bus sends, so a
                // concurrent delete/sweep can never publish before a journal
                // exit that committed earlier. It must NOT wrap the loop
                // below — the journal-bus publish and the alerts/usage/supply
                // observers await on unrelated resources; holding a global
                // lock across them serialised every journal ingest on the
                // Hub. Guards drop at the block's end, before those runs.
                let appended_chunk = {
                    let _settlement_order = state.settlement_publish_lock().await;
                    let chunk = state
                        .store
                        .append_journal_batch(
                            host_id.clone(),
                            instance_id.clone(),
                            next_seq,
                            events[range].to_vec(),
                        )
                        .await
                        .map_err(map_host_store)?;
                    for appended in &chunk {
                        // c-cardsettle: cards a terminal transaction
                        // invalidated commit with the append; tell followers
                        // immediately (a seq-less settlement control frame).
                        // Broadcast even for REPLAYED rows: a request replayed
                        // by a Node after the hello reconcile ended its owner
                        // is invalidated during replay (r7 item 1), and that
                        // notice has no other publication path (no fresh
                        // journal event follows it).
                        state.broadcast_settlement(&appended.settlement);
                    }
                    chunk
                };
                for appended in appended_chunk {
                    if !appended.replayed {
                        publish_journal(&state.bus, &appended.record);
                        crate::alerts::observe(state, &appended.record);
                        crate::usage_store::observe_journal(state, &appended.record).await;
                        crate::supply::observe_journal_text(state, &appended.record).await;
                        if crate::api_relay::journal_event_ends_instance(&appended.record.event) {
                            instance_terminated = true;
                        }
                    }
                    next_seq = Some(appended.record.seq.saturating_add(1));
                    last = Some(appended);
                }
            }
            // Normal instance exit/failure: revoke the gateway credential from
            // every proxy host that held an egress context for it (§7.6 / B.2).
            // There is no api.* frame on this path; the journal event is it.
            if instance_terminated {
                state
                    .api_relay
                    .revoke_instance_egress(state, &instance_id)
                    .await;
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
        method if HubNodeMethod::parse(method).is_some_and(HubNodeMethod::is_api) => {
            // D-048: the api.* frames are notifications with their own stream
            // table. They never enter the RPC pending map and answer nothing
            // (`Ok(None)` suppresses any id-less reply).
            //
            // `handle_notification` only enqueues onto the per-link drain
            // queue (try_send): it does not block the read loop on a
            // backpressured peer, so tty/journal/RPC frames keep flowing.
            let host_id = host_id.as_ref().ok_or(HubError::Unauthenticated)?;
            state
                .api_relay
                .handle_notification(state, host_id, method, params)
                .await;
            Ok(None)
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
/// longer *holds* is projected to `exited` with a Hub-authored diagnostic so
/// the loss is visible in the journal instead of silent.
///
/// "No longer holds" is read off the entry's own `lifecycle`, not off its
/// presence: an entry the Node lists as `exited`/`failed` is a confessed loss,
/// and treating it as "reported" would leave the Hub row shielding a process
/// that is already gone. Only an entry the Node claims is live proves the row
/// should survive.
///
/// Nodes that omit `instances` (a plain, non-daemon hello) report nothing, so
/// reconciliation only runs when an epoch change is actually observed and the
/// Node did send an inventory — otherwise a stateless Node would wipe rows it
/// simply never enumerates.
///
/// An *empty* inventory is the one answer that is destructive in the wrong
/// direction, because it reads as "I own nothing" and settles every row on the
/// host. It is honoured only when the Node also attests that it found its
/// instance store (`instanceStoreFound`): a `--data-dir` pointed at the wrong
/// path enumerates zero rows without that meaning anything, and trusting it
/// would exit every session on the host. Absent evidence, the rows are left
/// alone — the same hands-off answer an absent key gets.
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
        // c-cardsettle r8 item 2 (OA6): a SAME-epoch hello means the SAME Node
        // process reconnected after a contact loss — not a restart. The host
        // sweep may have marked still-running rows exited/host-lost while the
        // link was down. Revive the rows this inventory reports LIVE BEFORE
        // anything else (journal catch-up / card dispatch): host loss is
        // contact loss, never process end, and a new approval from the revived
        // process must stay pending and answerable.
        revive_live_inventory(state, host_id, params).await?;
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
    if reported.is_empty() && params.get("instanceStoreFound") != Some(&Value::Bool(true)) {
        tracing::warn!(
            %host_id,
            "node epoch changed and hello reported an empty inventory this node does not \
             vouch for; leaving instance rows untouched"
        );
        return Ok(());
    }
    let reported: Vec<String> = reported
        .iter()
        .filter(|item| !entry_is_terminal(item))
        .filter_map(|item| {
            item.get("id")
                .or_else(|| item.get("instanceId"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let lost = state
        .publish_settlement(state.store.reconcile_reported_instances(
            host_id.to_string(),
            reported,
            NODE_EPOCH_CHANGED.to_string(),
            true,
        ))
        .await?;
    for instance_id in lost {
        tracing::warn!(
            %host_id,
            %instance_id,
            "node epoch changed; instance lost"
        );
        // D-048: revoke any egress context the lost instance held on proxy
        // hosts; H must clear the credential.
        state
            .api_relay
            .revoke_instance_egress(state, &instance_id)
            .await;
        fail_workers_holding(state, host_id, &instance_id).await?;
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

/// Extract the instances a hello inventory reports as LIVE
/// (`ready`/`running`) as `(id, activity)` pairs, the input to
/// [`Store::revive_host_lost_instances`].
fn live_inventory_entries(params: &Value) -> Vec<(String, String)> {
    params
        .get("instances")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| !entry_is_terminal(item))
        .filter_map(|item| {
            let id = item
                .get("id")
                .or_else(|| item.get("instanceId"))
                .and_then(Value::as_str)?
                .to_string();
            // Only rows the Node claims are live revive a host-lost row.
            let lifecycle = item.get("lifecycle").and_then(Value::as_str).unwrap_or("");
            if !matches!(lifecycle, "ready" | "running") {
                return None;
            }
            let activity = match item
                .get("activity")
                .and_then(Value::as_str)
                .or_else(|| item.pointer("/activity/value").and_then(Value::as_str))
            {
                Some("waiting-interaction") => "blocked",
                Some(value @ ("idle" | "working" | "blocked" | "draining")) => value,
                _ => "idle",
            };
            Some((id, activity.to_string()))
        })
        .collect()
}

/// Same-epoch reconnect: revive host-lost rows the reconnecting Node still
/// reports live (c-cardsettle r8 item 2). Runs on BOTH daemon and plain
/// runtime hellos — the daemon inventory overlay
/// (`reconcile_daemon_instances`) already rewrites the rows it names, and the
/// host-lost predicate revival here is idempotent for those and the only
/// revival path for a non-daemon runtime link.
async fn revive_live_inventory(
    state: &AppState,
    host_id: &str,
    params: &Value,
) -> Result<(), HubError> {
    let live = live_inventory_entries(params);
    if live.is_empty() {
        return Ok(());
    }
    let revived = state
        .store
        .revive_host_lost_instances(host_id.to_string(), &live)
        .await?;
    for instance_id in revived {
        tracing::info!(
            %host_id,
            %instance_id,
            "same-epoch reconnect reports a host-lost instance live; revived without settling cards"
        );
    }
    Ok(())
}

/// The reason code a Node restart writes, on both sides of the wire.
///
/// `remuda_node::reclaim::NODE_EPOCH_CHANGED` is the same string, but the Hub
/// must not depend on the Node crate for it; the web turns exactly this
/// spelling into 「Node 重启，会话已中断」 plus Resume
/// (`web/src/lib/endReason.ts`), so a new spelling would silently lose
/// both the sentence and the affordance.
pub(crate) const NODE_EPOCH_CHANGED: &str = "node-epoch-changed";

/// True when an inventory entry says its process is gone.
///
/// A Node that omits the key is assumed alive: the inventory's contract is
/// "here is what I hold", and a Node too old to state a lifecycle must not
/// have its silence read as a confession.
fn entry_is_terminal(item: &Value) -> bool {
    matches!(
        item.get("lifecycle").and_then(Value::as_str),
        Some("exited" | "failed")
    )
}

/// Move every in-progress worker holding `instance_id` to blocked.
///
/// The instance row settling is only half the loss: a worker whose process is
/// gone but whose roster row still says `working` keeps counting as active,
/// keeps holding its port block, and tells its coordinator a dead session is
/// still making progress. Blocked-with-a-reason is what the dispatch contract
/// already means by "this worker cannot continue", so the transition reuses
/// [`WorkerState::Blocked`] rather than inventing a fourth state.
///
/// Only `dispatched`/`working` workers are rewritten. A `done` worker has
/// already reported a sha and a `blocked` one has already given a reason:
/// overwriting either would destroy a result the worker did deliver, and say
/// the session was lost when the work was finished. `retired` is terminal.
///
/// Scoped by `host_id`: instance ids are unique, but the guard keeps a future
/// id collision from letting one host's restart fail another host's rows.
async fn fail_workers_holding(
    state: &AppState,
    host_id: &str,
    instance_id: &str,
) -> Result<(), HubError> {
    let workers = state
        .store
        .list_workers_for_host(host_id.to_string())
        .await?;
    for worker in workers {
        if !worker.state.is_in_progress()
            || worker.instance_id.as_ref().map(|id| id.as_id().as_str()) != Some(instance_id)
        {
            continue;
        }
        tracing::warn!(
            %host_id,
            %instance_id,
            worker = %worker.name,
            "node epoch changed; failing the worker holding the lost instance"
        );
        state
            .store
            .mutate_worker(worker.meta.id.as_id().to_string(), |row| {
                row.state = WorkerState::Blocked {
                    reason: NODE_EPOCH_CHANGED.to_string(),
                };
                Ok(())
            })
            .await?;
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
        max_api_streams: remuda_protocol::default_max_api_streams(),
        api_chunk_bytes: remuda_protocol::default_api_chunk_bytes(),
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

/// c-cardsettle r6 item 2: drain a follower's missed settlement notices after
/// the broadcast ring reports `Lagged`. Pages the AUTHORITATIVE terminal rows
/// from durable storage in ASCENDING composite-cursor order, one bounded page
/// at a time, until a short page — so a backlog larger than one page loses no
/// older row. The cursor advances past EVERY row (sent or filtered out for a
/// per-instance follower). Notices are sent before the trailing gap frame, so
/// the client pins all of them before reconciling. A query error or a dead
/// writer returns Err and the caller closes the follower (never a bare gap).
///
/// r9 item 3 PUBLICATION-ORDER INVARIANT: the live broadcast path publishes
/// settlements in the order of the per-Hub monotonic `settlement_events.seq`
/// assigned inside each settling transaction (within one sweep the settle
/// SELECT orders rows by id so seqs ascend; across sweeps AUTOINCREMENT is a
/// total order even in the same millisecond). That ordering is what lets the
/// max-seq cursor and this strictly-forward `seq > cursor` recovery coexist:
/// a dropped low-seq row of an earlier batch always sits behind the cursor
/// and is recovered on the next drain. Keep both paths seq-ascending.
async fn drain_settlement_lag(
    store: &crate::store::Store,
    instance_ids: &[String],
    cursor: &mut Option<String>,
    out_tx: &mpsc::Sender<FollowMsg>,
) -> Result<(), ()> {
    // A lag drain always closes with the single gap frame.
    drain_settlement_pages(store, instance_ids, cursor, out_tx, true).await
}

/// Page every settlement strictly after `cursor` through `out_tx`, advancing
/// the cursor past EVERY row (including rows filtered out for a per-instance
/// follower), in ascending monotonic seq order. `trailing_gap` sends the
/// single settlement-backpressure gap after the pages — true for a Lagged
/// recovery, FALSE for the startup catch-up (the follower is not lagging, it
/// is closing the subscribe window).
async fn drain_settlement_pages(
    store: &crate::store::Store,
    instance_ids: &[String],
    cursor: &mut Option<String>,
    out_tx: &mpsc::Sender<FollowMsg>,
    trailing_gap: bool,
) -> Result<(), ()> {
    loop {
        let page = store
            .invalidated_interactions_after(cursor.clone())
            .await
            .map_err(|error| {
                tracing::error!(%error, "settlement lag recovery query failed; closing follower");
            })?;
        let short = page.len() < crate::store::SETTLEMENT_LAG_PAGE as usize;
        for (sid, iid, reason, seq_token) in page {
            if instance_ids.is_empty() || instance_ids.iter().any(|id| id == &sid) {
                let frame = json!({
                    "type": "settlement",
                    "instanceId": sid,
                    "interactionId": iid,
                    "state": "invalidated",
                    "reason": reason,
                })
                .to_string();
                out_tx.send(FollowMsg::Text(frame)).await.map_err(|_| ())?;
            }
            // Cursor advances past EVERY paged row, sent or filtered. The
            // token is the monotonic settlement_events.seq (r9 item 3); it
            // was issued by the store, so it parses.
            let seq = seq_token.parse::<i64>().map_err(|_| ())?;
            *cursor = Some(crate::store::Store::settlement_max_cursor(
                cursor.as_deref(),
                seq,
            ));
        }
        if short {
            break;
        }
    }
    if trailing_gap {
        let gap = json!({ "type": "gap", "reason": "settlement-backpressure" }).to_string();
        out_tx.send(FollowMsg::Text(gap)).await.map_err(|_| ())?;
    }
    Ok(())
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
    // c-cardsettle r9 item 4(a): read the durable seed BEFORE taking the
    // settlement-bus subscription. The old order (subscribe, then read) still
    // had a skip window: more than one ring of settlements committed between
    // the read and the pump's first recv makes the brand-new receiver return
    // Lagged with none of those notices consumed, and a drain seeded at the
    // post-commit max then excludes them (`seq > cursor`). With the seed read
    // FIRST, everything <= seed is covered by the initial list + connect
    // replay; the pump's startup catch-up (no gap) drains every settlement
    // committed AFTER the read, overlapping bus deliveries which the client
    // de-dupes by interaction id. No settlement can fall in between.
    let mut settlement_cursor: Option<String> = match state.store.max_settlement_cursor().await {
        Ok(cursor) => cursor,
        Err(error) => {
            tracing::error!(%error, "could not seed the settlement lag cursor; closing follower");
            return;
        }
    };
    // c-cardsettle: separate receiver on the dedicated settlement bus.
    let mut settlement_rx = state.settlement_bus.subscribe();
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
        // c-cardsettle r3 item 4: an UNFILTERED follower (the global inbox/
        // settlement socket) gets the very recent invalidated interactions
        // replayed on (re)connect, so a browser that misses the one-shot bus
        // notice across a page-navigation socket churn still drops the card
        // immediately instead of waiting on its (possibly halted) poll. The
        // client de-dupes; a fresh load also converges via the durable list.
        if instance_ids.is_empty()
            && let Ok(recent) = state.store.recent_invalidated_interactions().await
        {
            for (instance_id, interaction_id, reason) in recent {
                let frame = json!({
                    "type": "settlement",
                    "instanceId": instance_id,
                    "interactionId": interaction_id,
                    "state": "invalidated",
                    "reason": reason,
                })
                .to_string();
                if out_tx.send(FollowMsg::Text(frame)).await.is_err() {
                    return;
                }
            }
        }
        // c-cardsettle r9 item 4(a): startup catch-up. Drain every settlement
        // committed AFTER the pre-subscribe seed with NO trailing gap; rows at
        // or before the seed are already covered by the list + connect replay.
        // This closes the seed/subscribe window — a settlement committed in it
        // is delivered here even if the brand-new receiver Lagged before its
        // first recv (the bus copies overlap and are de-duped client-side).
        if drain_settlement_pages(
            &state.store,
            &instance_ids,
            &mut settlement_cursor,
            &out_tx,
            false,
        )
        .await
        .is_err()
        {
            return;
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
                            // c-cardsettle settlements arrive on the dedicated
                            // settlement_bus select arm below; the journal bus
                            // carries only journal/TTY frames now.
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
                // c-cardsettle: the dedicated settlement notice. Backpressure
                // here must AWAIT (never try_send/drop): this channel is low
                // traffic and the notice is the whole reason a poll-stopped
                // client drops the card. Each follower is an independent task,
                // so awaiting one slow writer cannot stall another.
                notice = settlement_rx.recv() => {
                    match notice {
                        Ok(notice) => {
                            let for_this_follower = instance_ids.is_empty()
                                || instance_ids.iter().any(|id| id == &notice.instance_id);
                            if for_this_follower {
                                let frame = json!({
                                    "type": "settlement",
                                    "instanceId": notice.instance_id,
                                    "interactionId": notice.interaction_id,
                                    "state": notice.state,
                                    "reason": notice.reason,
                                }).to_string();
                                if out_tx
                                    .send(FollowMsg::Text(frame))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            // r6 item 2: advance the delivery cursor on EVERY
                            // row the bus passes — including rows filtered out
                            // for this follower — so a later lag drain never
                            // re-scans or skips past it.
                            //
                            // r9 item 3: the key is the monotonic
                            // settlement_events.seq assigned in the settling
                            // transaction, not a (updated_at, id) composite,
                            // so same-millisecond cross-settle publication can
                            // never skip a row a follower missed. Lag recovery
                            // pages strictly forward from this cursor; see
                            // drain_settlement_lag.
                            settlement_cursor = Some(crate::store::Store::settlement_max_cursor(
                                settlement_cursor.as_deref(),
                                notice.seq,
                            ));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // c-cardsettle r4 item 6 / r6 item 2: drain every
                            // missed settlement (bounded ASC pages until a
                            // short page) BEFORE the gap frame; a query error
                            // closes the follower.
                            if drain_settlement_lag(
                                &state.store,
                                &instance_ids,
                                &mut settlement_cursor,
                                &out_tx,
                            )
                            .await
                            .is_err()
                            {
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
    // Read the row once: it carries both the host to attach through and the
    // lifecycle that decides whether the cached screen below is a live view or
    // the remains of a session that has already ended.
    let record = state
        .store
        .get_instance(instance_id.to_string())
        .await
        .ok()
        .flatten();
    let host_id = record.as_ref().map(|instance| instance.host_id.clone());
    // Why the cached screen is not live, when it reaches the browser. A row
    // that is already terminal explains itself. Otherwise the Hub has no live
    // screen to show — `tty.attach` went unanswered, or it answered without
    // bytes — and reporting the link, rather than the session, is the claim
    // that is actually supported: the session may well be alive on the far
    // side of a broken link.
    let fallback_reason = match record.as_ref().map(|row| row.lifecycle.as_str()) {
        Some("exited" | "failed" | "closed" | "terminated") => "instance-gone",
        _ => "node-link-unavailable",
    };
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
                    "streamId": stream_id,
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
                    if let Some(frame) =
                        encode_output_frame(stream_id.as_deref().unwrap_or(""), offset, &bytes)
                    {
                        out_tx
                            .send(FollowMsg::Binary(frame))
                            .await
                            .map_err(|_| ())?;
                        return Ok(());
                    }
                }
                // The attach succeeded, so this session is *live* — it simply
                // had no backlog to replay (a fresh PTY, or a Node that had
                // nothing buffered yet). Returning here is the whole point of
                // this branch: falling through would serve the Hub's cache and
                // label a running session stale, freezing stdin on a terminal
                // that is answering. The stream is bound above, so live bytes
                // reach this follower as ordinary binary frames.
                if stream_id.is_some() {
                    return Ok(());
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
    //
    // The bytes are *stale*: they are whatever this Hub last saw before the
    // link went away, and the browser must not paint them as a live screen
    // (2026-09-18: a frozen 17:36 frame kept its 运行中 header for a session
    // whose process had been dead for an hour). So the snapshot carries the
    // capture time and why it is not live. The binary path is deliberately not
    // taken even when a stream id is known: `tty.frame` has nowhere to put the
    // metadata, and an unlabelled frame is exactly the bug.
    let cached = state.tty.snapshot(instance_id);
    if cached.is_empty() {
        return Ok(());
    }
    let message = FollowMsg::Text(
        json!({
            "type": "tty.snapshot",
            "instanceId": instance_id,
            "source": "hub-cache",
            "dataBase64": encode_b64(&cached.bytes),
            // Absent for bytes cached by a Hub too old to have stamped them;
            // the web must then say the age is unknown rather than guess.
            "capturedAt": cached.captured_at,
            "reason": fallback_reason,
        })
        .to_string(),
    );
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
    let page = state
        .store
        .read_journal(instance_id.to_string(), 0, None)
        .await?;
    Ok(json!({
        "type": "snapshot",
        "instanceId": instance_id,
        "asOfSeq": page.durable_seq.to_string(),
        "durableSeq": page.durable_seq.to_string(),
        "fromSeq": page.from_seq.map(|seq| seq.to_string()),
        "reachedAfterSeq": page.reached_after_seq,
        "events": page.events,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SETTLEMENT_LAG_PAGE, Store};
    use rusqlite::params;

    /// Insert `count` rows into the settlement_events log, optionally one
    /// 2-hours-old lost row and one still-pending interaction (which has NO
    /// log entry and must never drain).
    async fn seed_backlog(
        store: &Store,
        count: u32,
        with_aged_lost: bool,
        with_pending: bool,
    ) -> Vec<String> {
        store
            .run_named("r9_seed_settlement_backlog", move |conn| {
                let mut ids = Vec::new();
                {
                    let mut stmt = conn.prepare(
                        "INSERT INTO settlement_events
                            (interaction_id, instance_id, state, reason, created_at)
                         VALUES (?1, 'ins_lag', 'invalidated', 'generation-ended', ?2)",
                    )?;
                    if with_aged_lost {
                        stmt.execute(params!["int_lag_AGED", "2026-09-01T00:00:00.000Z"])?;
                    }
                    for n in 0..count {
                        let id = format!("int_lag_{n:05}");
                        // Proper RFC3339 stamps; the backlog stays in the
                        // present window. Seq, not the stamp, orders the drain.
                        let stamp = format!("2026-10-07T12:{:02}:{:02}.000Z", n / 60, n % 60);
                        stmt.execute(params![id, stamp])?;
                        ids.push(format!("int_lag_{n:05}"));
                    }
                }
                if with_pending {
                    conn.execute(
                        "INSERT INTO interactions
                            (id, instance_id, host_id, kind, state, blocking,
                             payload_json, created_at, updated_at)
                         VALUES ('int_lag_PENDING', 'ins_lag', 'hst_lag', 'approval', 'pending', 1,
                                 '{}', '2026-10-07T13:00:00.000Z', '2026-10-07T13:00:00.000Z')",
                        [],
                    )?;
                }
                Ok(ids)
            })
            .await
            .expect("seed backlog")
    }

    /// c-cardsettle r6 item 2: after a broadcast Lagged the recovery drains a
    /// backlog LARGER than one page ASCENDING — including an hours-old lost
    /// settlement — sends each notice exactly once BEFORE the single gap
    /// frame, never includes a pending row, and leaves the cursor past the tail
    /// so a second drain sends only another gap.
    #[tokio::test]
    async fn settlement_lag_drain_walks_every_page_ascending_without_skipping() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open(dir.path()).expect("store");
        let expected_rows = SETTLEMENT_LAG_PAGE + 25;
        seed_backlog(&store, expected_rows, true, true).await;

        // Bounded channel with a concurrently draining receiver: every notice
        // is observed (the helper awaits a slow writer just like the real
        // follower pump).
        let (tx, mut rx) = mpsc::channel::<FollowMsg>(8);
        let drainer = tokio::spawn(async move {
            let mut cursor: Option<String> = None;
            drain_settlement_lag(&store, &[], &mut cursor, &tx)
                .await
                .expect("drain");
            (store, cursor)
        });

        let mut settlements: Vec<(String, String, String)> = Vec::new();
        let mut gaps = 0;
        while let Some(msg) = rx.recv().await {
            match msg {
                FollowMsg::Text(text) => {
                    let frame: Value = serde_json::from_str(&text).expect("json");
                    match frame["type"].as_str() {
                        Some("settlement") => settlements.push((
                            frame["instanceId"].as_str().unwrap().to_owned(),
                            frame["interactionId"].as_str().unwrap().to_owned(),
                            frame["reason"].as_str().unwrap().to_owned(),
                        )),
                        Some("gap") => gaps += 1,
                        other => panic!("unexpected frame type {other:?}"),
                    }
                }
                FollowMsg::Binary(_) => panic!("no binary frames"),
            }
        }
        let (_store, cursor) = drainer.await.expect("drain task");

        // Aged lost row + every backlog row, exactly once, no pending row.
        assert_eq!(
            settlements.len(),
            expected_rows as usize + 1,
            "the whole multi-page backlog and the old lost row drain"
        );
        let ids: Vec<&str> = settlements.iter().map(|(_, id, _)| id.as_str()).collect();
        assert_eq!(
            ids.first(),
            Some(&"int_lag_AGED"),
            "the hours-old lost row drains first"
        );
        assert!(
            !ids.contains(&"int_lag_PENDING"),
            "pending rows never drain"
        );
        // After the aged row the backlog goes out in seed (timestamp) order.
        let tail: Vec<&str> = ids[1..].to_vec();
        let mut sorted_tail = tail.clone();
        sorted_tail.sort_unstable();
        assert_eq!(
            tail, sorted_tail,
            "notices go out oldest-first across pages"
        );
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "no settlement is sent twice");
        assert!(
            settlements
                .iter()
                .all(|(_, _, reason)| reason == "generation-ended"),
            "reason carried on every notice: {settlements:?}"
        );
        assert_eq!(gaps, 1, "exactly one gap frame follows the full drain");
        assert!(cursor.is_some(), "the cursor advanced past the tail");
    }

    /// c-cardsettle r9 item 3: cross-`settle` publication ORDER. A follower
    /// that has already drained part of a FIRST settlement batch (its cursor
    /// ends MID the settlement log) must not be able to permanently skip rows
    /// of a SECOND settle whose notices overrun the broadcast ring.
    /// Concretely:
    ///   1. settle A (5 rows); the follower receives 3 live, cursor mid-A.
    ///   2. settle B (70 rows > the 64-notice ring) with NO live delivery
    ///      (its out channel is dropped, as a backpressured socket behaves).
    ///   3. resume draining from the mid-A cursor: all remaining A rows AND all
    ///      B rows must be recovered, in ascending settlement seq order, each
    ///      once, before the gap.
    ///
    /// The per-Hub AUTOINCREMENT seq assigned in the settling transaction is a
    /// total order even when the two settles share a millisecond and their
    /// interaction ids interleave, so a strictly-forward seq cursor can never
    /// skip a row of the earlier batch.
    #[tokio::test]
    async fn settlement_cursor_mid_first_batch_recovers_a_later_overring_sweep() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open(dir.path()).expect("store");
        let host = format!("hst_{}", uuid::Uuid::now_v7());
        store
            .run_named("r8_enroll_host", {
                let host = host.clone();
                move |conn| {
                    let now = crate::config::now_rfc3339();
                    conn.execute(
                        "INSERT INTO hosts (id, label, token_hash, state, last_seen_at,
                            node_version, cli_json, capabilities_json, created_at, transport,
                            labels_json, max_instances, hostname)
                         VALUES (?1, ?1, 'x', 'online', ?2, '0.1.0-test', '[]', '{}', ?2,
                                 'outbound-wss', '[]', 8, 'r8-order')",
                        rusqlite::params![host, now],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("enroll");

        // Seed one live instance per settle with pending cards, through the
        // same public APIs the node socket handler uses.
        let seed = |n: u32, cards: u32| {
            let store = store.clone();
            let host = host.clone();
            async move {
                let instance_id = format!("ins_r8order_{n}");
                store
                    .ensure_instance(host.clone(), instance_id.clone())
                    .await
                    .expect("ensure instance");
                store
                    .append_journal(
                        host.clone(),
                        instance_id.clone(),
                        None,
                        json!({
                            "kind": "lifecycle",
                            "payload": {
                                "type": "entity", "entityType": "instance", "state": "ready"
                            }
                        }),
                    )
                    .await
                    .expect("ready entity");
                let mut ids = Vec::new();
                for c in 0..cards {
                    let id = format!("int_r8order_{n}_{c:05}");
                    store
                        .append_journal(
                            host.clone(),
                            instance_id.clone(),
                            None,
                            json!({
                                "kind": "interaction.requested",
                                "payload": {
                                    "interactionKind": "approval",
                                    "interaction": {
                                        "id": id,
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
                                            "description": "r8 order",
                                            "options": []
                                        }
                                    }
                                }
                            }),
                        )
                        .await
                        .expect("approval request");
                    ids.push(id);
                }
                (instance_id, ids)
            }
        };
        // Batch A settles FIRST, then B. The seqs are what order publication
        // (A's five before B's seventy) even though B is the bigger sweep and
        // both settle at the wall clock's same millisecond.
        let (inst_a, mut cards_a) = seed(2, 5).await;
        let (inst_b, cards_b) = seed(1, 70).await;

        // Settle A, then B; B never publishes live to this follower.
        store
            .settle_instance_exited(inst_a.clone(), "r9-order-a".into())
            .await
            .expect("settle A");
        store
            .settle_instance_exited(inst_b.clone(), "r9-order-b".into())
            .await
            .expect("settle B");

        // PART 1: the follower received the first three A rows live, so its
        // durable cursor is the seq token on the third row.
        let page = store
            .invalidated_interactions_after(None)
            .await
            .expect("page");
        let first_three: Vec<String> = page
            .iter()
            .take(3)
            .map(|(_, id, _, _)| id.clone())
            .collect();
        let mid_cursor = page
            .iter()
            .find(|(_, id, _, _)| id == &first_three[2])
            .expect("third row present")
            .3
            .clone();

        // PART 2: fresh drain from the MID cursor recovers the rest in order.
        // Concurrently receive: the batch exceeds the channel capacity, so the
        // drain task must run while this test reads (mirrors the real pump).
        let mut cursor = Some(mid_cursor);
        let (tx, mut rx) = mpsc::channel::<FollowMsg>(16);
        let drainer =
            tokio::spawn(async move { drain_settlement_lag(&store, &[], &mut cursor, &tx).await });
        let mut recovered = Vec::new();
        let mut gaps = 0;
        while let Some(FollowMsg::Text(text)) = rx.recv().await {
            let frame: Value = serde_json::from_str(&text).expect("json");
            match frame["type"].as_str() {
                Some("settlement") => {
                    recovered.push(frame["interactionId"].as_str().unwrap().to_string())
                }
                Some("gap") => gaps += 1,
                other => panic!("unexpected frame {other:?}"),
            }
        }
        drainer.await.expect("drain task").expect("drain ok");

        // Expect A's remaining two followed by all 70 B rows, total order.
        cards_a.sort();
        let mut expected: Vec<String> = first_three.to_vec();
        expected.extend(recovered.iter().cloned());
        let mut want = Vec::new();
        want.append(&mut cards_a.clone());
        let mut cards_b = cards_b;
        cards_b.sort();
        want.append(&mut cards_b);
        assert_eq!(
            expected, want,
            "mid-cursor drain completes both batches in order"
        );
        assert_eq!(gaps, 1, "one gap closes the drain");
        // No duplicates across the two drain phases.
        let mut unique = expected.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), want.len(), "no row delivered twice");
        let _ = inst_a;
        let _ = inst_b;
    }

    /// r6 item 2: a per-instance follower advances its cursor past rows
    /// filtered out for it, so those rows are neither sent nor re-scanned on a
    /// follow-up drain.
    #[tokio::test]
    async fn settlement_lag_drain_filters_by_instance_and_advances_past_filtered() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open(dir.path()).expect("store");
        store
            .run_named("r9_seed_two_instances", |conn| {
                conn.execute(
                    "INSERT INTO settlement_events
                        (interaction_id, instance_id, state, reason, created_at)
                     VALUES
                        ('int_OWNED', 'ins_owned', 'invalidated',
                         'generation-ended', '2026-10-07T12:00:01.000Z'),
                        ('int_OTHER', 'ins_other', 'invalidated',
                         'generation-ended', '2026-10-07T12:00:02.000Z')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("seed");
        let (tx, rx) = mpsc::channel::<FollowMsg>(8);
        let filter = vec!["ins_owned".to_string()];
        let mut cursor: Option<String> = None;
        drain_settlement_lag(&store, &filter, &mut cursor, &tx)
            .await
            .expect("drain");
        drop(tx);
        let mut ids = Vec::new();
        let mut gaps = 0;
        let mut rx = rx;
        while let Some(FollowMsg::Text(text)) = rx.recv().await {
            let frame: Value = serde_json::from_str(&text).unwrap();
            match frame["type"].as_str() {
                Some("settlement") => ids.push(frame["interactionId"].as_str().unwrap().to_owned()),
                Some("gap") => gaps += 1,
                _ => {}
            }
        }
        assert_eq!(
            ids,
            vec!["int_OWNED"],
            "only the owned instance's row is sent"
        );
        assert_eq!(gaps, 1);
        // The cursor is past BOTH rows (the filtered one advanced it): a second
        // drain finds nothing beyond another gap.
        let (tx2, mut rx2) = mpsc::channel::<FollowMsg>(8);
        drain_settlement_lag(&store, &filter, &mut cursor, &tx2)
            .await
            .expect("drain 2");
        drop(tx2);
        let mut settlements_2 = 0;
        let mut gaps_2 = 0;
        while let Some(FollowMsg::Text(text)) = rx2.recv().await {
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["type"] == "settlement" {
                settlements_2 += 1;
            } else if frame["type"] == "gap" {
                gaps_2 += 1;
            }
        }
        assert_eq!(settlements_2, 0, "the filtered row is never re-sent");
        assert_eq!(gaps_2, 1);
    }

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
