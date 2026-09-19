//! In-band model-API relay (D-047 / D-048): per-instance loopback listener on
//! the worker host W, `api.*` frames over the existing Hub↔Node link, and the
//! pinned-origin egress client on the proxy host H.
//!
//! Layout:
//! * [`policy`] — bearer mint/verify, header and path allowlists, bind policy;
//! * [`listener`] — the per-instance HTTP servers (worker loopback listener,
//!   optional proxy direct-net listener) and the direct-net client;
//! * [`egress`] — the proxy host's streaming gateway call, response
//!   coalescing and the timeout ladder;
//! * this file — [`ApiRelayState`] (Node-lifetime registry) and the per-link
//!   [`LinkBroker`] that demuxes `api.*` notifications away from the JSON-RPC
//!   dispatch table, exactly as D-048 requires.
//!
//! The frames are notifications with their own stream table: they never enter
//! the 32-slot RPC pending map that `instance.create` and `tty.write` depend
//! on. One broker is attached per carrier session by the transport loops
//! (`transport/wss.rs`, `stdio.rs`, `daemon.rs`); a per-instance listener
//! reaches the Hub only through the broker currently attached here.

// The proxy-side egress API (the H half) is production code but its only
// production caller — the Hub relay session that pushes an
// [`egress::EgressContext`] before routing an `api.open` here — is the Hub
// task's wiring (api-routing task 2). The Node itself reaches it today
// through in-crate tests, so silence dead-code lints for the H half rather
// than delete the surface the next task plugs into.
#[allow(dead_code)]
pub mod egress;
pub(crate) mod listener;
mod policy;

use crate::NodeError;
use bytes::Bytes;
use remuda_protocol::hubnode::{
    ApiBodyParams, ApiCancelParams, ApiChunkParams, ApiCreditParams, ApiEndError, ApiEndParams,
    ApiHeadParams, ApiOpenParams, METHOD_API_BODY, METHOD_API_CANCEL, METHOD_API_CHUNK,
    METHOD_API_CREDIT, METHOD_API_END, METHOD_API_HEAD, METHOD_API_OPEN,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{Semaphore, mpsc};

/// Live `api.*` streams allowed per link (§7.6 default).
const MAX_LINK_STREAMS: usize = 8;
/// Live relay streams allowed per instance (§7.6 default).
const MAX_INSTANCE_STREAMS: usize = 2;
/// Initial producer window, in chunks: at most this many chunks may be
/// unacknowledged on one stream before the producer stalls for `api.credit`.
pub(crate) const INITIAL_CREDIT_WINDOW: u32 = 4;
/// Outbound notification channel for one carrier session. Credits cap the real
/// in-flight chunks at 4 per stream, so this only needs to absorb control
/// frames (open/head/cancel/credit/end) alongside them.
const LINK_CHANNEL_CAPACITY: usize = 64;
/// Per-stream channel between the demuxer and the stream owner. A reader that
/// applies backpressure here is the credit mechanism working.
const STREAM_EVENT_CAPACITY: usize = 32;

/// Hard lifetime of one relayed stream (§7.6): 30 minutes.
pub(crate) const STREAM_HARD_CAP: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// One demuxed `api.*` frame addressed to a stream owner.
///
/// The worker-side listener consumes [`Self::Head`] / [`Self::Chunk`] /
/// [`Self::End`]; the proxy-side egress consumes [`Self::Body`]. `credit` and
/// `cancel` are valid in either direction and reach whichever side owns the
/// stream.
#[derive(Debug)]
pub(crate) enum InboundEvent {
    /// `api.head`: response status and headers committed before any body.
    Head(ApiHeadParams),
    /// `api.chunk`: one response body chunk.
    Chunk(ApiChunkParams),
    /// `api.end`: terminal frame, success or error.
    End(ApiEndParams),
    /// `api.credit`: more producer permits.
    Credit(ApiCreditParams),
    /// `api.cancel`: the other side abandoned the stream.
    Cancel(ApiCancelParams),
    /// `api.body`: one request body chunk.
    Body(ApiBodyParams),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Stream opened locally by the worker-side listener.
    Worker,
    /// Stream opened by the Hub for this Node to egress (proxy role).
    Proxy,
}

struct Slot {
    instance_id: String,
    role: Role,
    tx: mpsc::Sender<InboundEvent>,
}

/// Per-stream sending half: frames onto the carrier plus the credit window.
///
/// "Gated" sends (body on W, response chunks on H) consume one permit each and
/// stall once [`INITIAL_CREDIT_WINDOW`] chunks are unacknowledged; inbound
/// `api.credit` calls [`Self::grant`]. Control frames bypass the window.
///
/// Cloneable: one copy sends data while an event router on the same stream
/// grants credits through another. Both share the one permit pool. These are
/// the only strong senders into the carrier's outbound channel, which is why
/// the broker itself holds none.
#[derive(Clone)]
pub(crate) struct Outbox {
    outbound: mpsc::Sender<Value>,
    stream_id: String,
    window: Arc<Semaphore>,
}

impl std::fmt::Debug for Outbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Outbox")
            .field("stream_id", &self.stream_id)
            .finish_non_exhaustive()
    }
}

impl Outbox {
    /// Stream this outbox addresses.
    #[must_use]
    pub(crate) fn stream_id(&self) -> &str {
        &self.stream_id
    }

    /// Send a notification that is not data (`api.open`, `api.head`,
    /// `api.credit`, `api.cancel`, `api.end`).
    pub(crate) async fn send_notification<T: Serialize>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<(), NodeError> {
        let frame = notification(method, params)?;
        self.outbound
            .send(frame)
            .await
            .map_err(|_| NodeError::Disconnected)
    }

    /// Send one data chunk, consuming one credit permit. The permit is
    /// forgotten rather than dropped, because it represents the consumer's
    /// budget and is only returned through an inbound `api.credit`.
    pub(crate) async fn send_gated<T: Serialize>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<(), NodeError> {
        let permit = self
            .window
            .acquire()
            .await
            .expect("credit window semaphore is never closed");
        permit.forget();
        self.send_notification(method, params).await
    }

    /// Return `chunks` permits after the consumer drained them.
    pub(crate) fn grant(&self, chunks: u32) {
        self.window
            .add_permits(usize::try_from(chunks).unwrap_or(0));
    }
}

/// Build a JSON-RPC notification frame (no `id`) — the only shape `api.*`
/// frames ever take.
fn notification<T: Serialize>(method: &str, params: &T) -> Result<Value, NodeError> {
    Ok(serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": serde_json::to_value(params).map_err(|error| NodeError::Transport(error.to_string()))?,
    }))
}

/// One carrier session's `api.*` demuxer and outbound channel.
///
/// Created by a transport loop when a Hub session starts, drained into the
/// socket via the receiver returned by [`ApiRelayState::attach_link`], fed
/// inbound frames through [`Self::handle_frame`], and failed wholesale on
/// reconnect/close with [`Self::fail_all`]. Dropping the broker detaches it
/// from the state.
///
/// The broker reaches its carrier channel only through per-stream
/// [`Outbox`]s: with no live streams it holds no strong sender, so a dead
/// session's write arm wakes on a closed channel rather than parking on a
/// sender the broker kept alive itself (which would wedge controller
/// takeover).
pub struct LinkBroker {
    id: u64,
    state: Arc<ApiRelayState>,
    streams: Mutex<HashMap<String, Slot>>,
}

impl std::fmt::Debug for LinkBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinkBroker")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl LinkBroker {
    /// Open a worker-side stream on this link, enforcing the per-link and
    /// per-instance live-stream caps. Returns the sending half and a receiver
    /// for the stream's inbound events.
    pub(crate) fn open_worker_stream(
        self: &Arc<Self>,
        instance_id: &str,
    ) -> Result<(String, Outbox, mpsc::Receiver<InboundEvent>), NodeError> {
        let stream_id = format!("strm_{}", uuid::Uuid::new_v4().simple());
        let (tx, rx) = mpsc::channel(STREAM_EVENT_CAPACITY);
        {
            let mut streams = self
                .streams
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut for_instance = 0usize;
            for slot in streams.values() {
                if slot.instance_id == instance_id {
                    for_instance += 1;
                }
            }
            if for_instance >= MAX_INSTANCE_STREAMS {
                return Err(NodeError::InvalidRequest(
                    "api relay instance stream limit reached".into(),
                ));
            }
            if streams.len() >= MAX_LINK_STREAMS {
                return Err(NodeError::InvalidRequest(
                    "api relay link stream limit reached".into(),
                ));
            }
            streams.insert(
                stream_id.clone(),
                Slot {
                    instance_id: instance_id.to_owned(),
                    role: Role::Worker,
                    tx,
                },
            );
        }
        let outbox = Outbox {
            outbound: self.carrier_sender()?,
            stream_id: stream_id.clone(),
            window: Arc::new(Semaphore::new(
                usize::try_from(INITIAL_CREDIT_WINDOW).unwrap_or(4),
            )),
        };
        Ok((stream_id, outbox, rx))
    }

    /// Strong carrier sender for a new stream. `None` when the session is gone.
    fn carrier_sender(&self) -> Result<mpsc::Sender<Value>, NodeError> {
        self.state
            .carrier_sender_for(self.id)
            .ok_or(NodeError::Disconnected)
    }

    /// Register the proxy half of a stream the Hub opened (inbound
    /// `api.open`). Returns `None` for a duplicate `streamId`: the frame is
    /// consumed but ignored, matching the object-broker's idempotence.
    fn register_proxy_stream(
        self: &Arc<Self>,
        open: &ApiOpenParams,
    ) -> Option<(Outbox, mpsc::Receiver<InboundEvent>)> {
        let (tx, rx) = mpsc::channel(STREAM_EVENT_CAPACITY);
        {
            let mut streams = self
                .streams
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if streams.len() >= MAX_LINK_STREAMS || streams.contains_key(&open.stream_id) {
                return None;
            }
            // Defense in depth on H too: the Hub is supposed to cap at two
            // streams per instance, but a proxy Node must not rely on that.
            let for_instance = streams
                .values()
                .filter(|slot| slot.instance_id == open.instance_id)
                .count();
            if for_instance >= MAX_INSTANCE_STREAMS {
                return None;
            }
            streams.insert(
                open.stream_id.clone(),
                Slot {
                    instance_id: open.instance_id.clone(),
                    role: Role::Proxy,
                    tx,
                },
            );
        }
        Some((
            Outbox {
                outbound: match self.carrier_sender() {
                    Ok(sender) => sender,
                    Err(_) => return None,
                },
                stream_id: open.stream_id.clone(),
                window: Arc::new(Semaphore::new(
                    usize::try_from(INITIAL_CREDIT_WINDOW).unwrap_or(4),
                )),
            },
            rx,
        ))
    }

    /// Remove a finished stream's slot.
    fn close_stream(&self, stream_id: &str) {
        self.streams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(stream_id);
    }

    /// Feed one inbound carrier frame. Returns true when it was an `api.*`
    /// notification addressed to this facility — the transport loops call this
    /// *before* JSON-RPC dispatch, so these frames never reach the RPC table.
    ///
    /// Async on purpose: when a stream owner is applying backpressure this
    /// awaits in the carrier's read arm, which propagates the pressure back to
    /// the Hub instead of dropping a data chunk the credit system said was
    /// allowed. Frames are demuxed one at a time in arrival order.
    pub(crate) async fn handle_frame(self: &Arc<Self>, frame: &Value) -> bool {
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return false;
        };
        let Some(params) = frame.get("params").cloned() else {
            return false;
        };
        match method {
            METHOD_API_OPEN => self.handle_open(params),
            METHOD_API_BODY => {
                self.deliver(params, Some(Role::Proxy), |p| {
                    serde_json::from_value(p).ok().map(InboundEvent::Body)
                })
                .await
            }
            METHOD_API_HEAD => {
                self.deliver(params, Some(Role::Worker), |p| {
                    serde_json::from_value(p).ok().map(InboundEvent::Head)
                })
                .await
            }
            METHOD_API_CHUNK => {
                self.deliver(params, Some(Role::Worker), |p| {
                    serde_json::from_value(p).ok().map(InboundEvent::Chunk)
                })
                .await
            }
            METHOD_API_END => {
                // Terminal: deliver to whichever role owns the stream, then
                // free the slot.
                let stream_id = params
                    .get("streamId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.deliver(params, None, |p| {
                    serde_json::from_value(p).ok().map(InboundEvent::End)
                })
                .await;
                if let Some(stream_id) = stream_id {
                    self.close_stream(&stream_id);
                }
            }
            // Bidirectional: reach whichever role owns the stream.
            METHOD_API_CANCEL => {
                self.deliver(params, None, |p| {
                    serde_json::from_value(p).ok().map(InboundEvent::Cancel)
                })
                .await
            }
            METHOD_API_CREDIT => {
                self.deliver(params, None, |p| {
                    serde_json::from_value(p).ok().map(InboundEvent::Credit)
                })
                .await
            }
            _ => return false,
        }
        true
    }

    /// Demux one typed frame to the slot owning its stream. Frames for unknown
    /// stream ids are consumed and dropped: a late chunk for a dead stream
    /// must not become an RPC error.
    async fn deliver(
        &self,
        params: Value,
        expected: Option<Role>,
        parse: impl FnOnce(Value) -> Option<InboundEvent>,
    ) {
        let Some(stream_id) = params
            .get("streamId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            tracing::debug!("api frame without streamId; ignored");
            return;
        };
        let tx = {
            let streams = self
                .streams
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match streams.get(&stream_id) {
                Some(slot) if expected.is_none_or(|role| slot.role == role) => slot.tx.clone(),
                Some(_) => {
                    tracing::debug!(stream_id, ?expected, "api frame for wrong role; ignored");
                    return;
                }
                None => {
                    tracing::debug!(stream_id, "api frame for unknown stream; ignored");
                    return;
                }
            }
        };
        let Some(event) = parse(params) else {
            tracing::warn!(stream_id, "malformed api frame; ignored");
            return;
        };
        // Awaited outside the lock. The producer is credit-gated to a few
        // unacknowledged chunks, so this only parks when the owner has truly
        // stopped draining; backing up the read arm is then the correct answer.
        if tx.send(event).await.is_err() {
            tracing::debug!(stream_id, "api stream owner gone; frame ignored");
        }
    }

    /// Inbound `api.open`: this Node is the proxy host H for the stream. Spawn
    /// the egress against the per-instance gateway context the Hub pushed.
    fn handle_open(self: &Arc<Self>, params: Value) {
        let Ok(open) = serde_json::from_value::<ApiOpenParams>(params) else {
            tracing::warn!("malformed api.open; ignored");
            return;
        };
        let Some((outbox, events)) = self.register_proxy_stream(&open) else {
            return;
        };
        let broker = Arc::clone(self);
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            egress::serve_inbound(&state, &broker, open, outbox, events).await;
        });
    }

    /// Fail every live stream on this link (socket loss / reconnect). The
    /// listener maps this to a 503 with an Anthropic-shaped body; retries ride
    /// the next socket as new streams.
    pub(crate) fn fail_all(&self, code: &'static str, message: &'static str) {
        let slots: Vec<(String, mpsc::Sender<InboundEvent>)> = self
            .streams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain()
            .map(|(id, slot)| (id, slot.tx))
            .collect();
        for (stream_id, tx) in slots {
            let _ = tx.try_send(InboundEvent::End(ApiEndParams {
                stream_id: stream_id.clone(),
                error: Some(ApiEndError {
                    code: code.into(),
                    message: message.into(),
                }),
                bytes_up: 0,
                bytes_down: 0,
                ms: 0,
            }));
        }
    }

    /// Cancel and remove every stream owned by one instance (its listener was
    /// revoked at exit).
    fn cancel_instance(&self, instance_id: &str) {
        let owned: Vec<String> = self
            .streams
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter_map(|(id, slot)| (slot.instance_id == instance_id).then_some(id.clone()))
            .collect();
        for stream_id in owned {
            if let Some(slot) = self
                .streams
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&stream_id)
            {
                let _ = slot.tx.try_send(InboundEvent::End(ApiEndParams {
                    stream_id,
                    error: Some(ApiEndError {
                        code: remuda_protocol::hubnode::API_ERROR_INSTANCE_GONE.into(),
                        message: "instance exited".into(),
                    }),
                    bytes_up: 0,
                    bytes_down: 0,
                    ms: 0,
                }));
            }
        }
    }
}

impl Drop for LinkBroker {
    fn drop(&mut self) {
        self.fail_all(
            remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST,
            "hub link closed",
        );
        if let Ok(mut guard) = self.state.link.write()
            && guard.as_ref().is_some_and(|active| active.id == self.id)
        {
            *guard = None;
        }
    }
}

/// Node-lifetime registry for relay listeners, proxy egress contexts, and the
/// currently attached carrier link. One per [`crate::DevNode`].
pub struct ApiRelayState {
    /// The live broker and the strong half of its carrier channel. The
    /// transport loop owns the receiver; this sender stays alive for the whole
    /// session so every opened stream can send, and is dropped on replacement
    /// or detach to close that receiver.
    link: RwLock<Option<ActiveLink>>,
    next_link_id: AtomicU64,
    /// Per-instance listeners that authenticated local requests may enter.
    instances: Mutex<BTreeMap<String, Arc<listener::RelayInstance>>>,
    /// Proxy-side gateway contexts, keyed by instance id, pushed by the Hub
    /// before it routes an `api.open` here and revoked at instance end.
    egress: RwLock<HashMap<String, Arc<egress::EgressContext>>>,
    /// Shared direct-net / probe client. Redirects are never followed: a
    /// gateway redirect to another origin must not carry the credential.
    http: reqwest::Client,
}

struct ActiveLink {
    id: u64,
    broker: Arc<LinkBroker>,
    sender: mpsc::Sender<Value>,
}

impl std::fmt::Debug for ApiRelayState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiRelayState")
            .finish_non_exhaustive()
    }
}

impl ApiRelayState {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(egress::EGRESS_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            link: RwLock::new(None),
            next_link_id: AtomicU64::new(1),
            instances: Mutex::new(BTreeMap::new()),
            egress: RwLock::new(HashMap::new()),
            http,
        })
    }

    /// Attach a carrier session's broker. The returned receiver is drained by
    /// the transport loop into its socket. Attaching replaces a previous
    /// session (a stale broker then fails its own streams on drop).
    pub(crate) fn attach_link(self: &Arc<Self>) -> (Arc<LinkBroker>, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(LINK_CHANNEL_CAPACITY);
        let id = self.next_link_id.fetch_add(1, Ordering::Relaxed);
        let broker = Arc::new(LinkBroker {
            id,
            state: Arc::clone(self),
            streams: Mutex::new(HashMap::new()),
        });
        // Swap under the lock, but drop the replaced session *after* the
        // guard is released: a dead broker's Drop detaches through this same
        // lock, and `std::sync::RwLock` is not reentrant.
        let previous = {
            let mut guard = self
                .link
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.replace(ActiveLink {
                id,
                broker: Arc::clone(&broker),
                sender: tx,
            })
        };
        drop(previous);
        (broker, rx)
    }

    /// The broker of the currently attached carrier session, if any.
    pub(crate) fn active_link(&self) -> Option<Arc<LinkBroker>> {
        self.link
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|active| Arc::clone(&active.broker))
    }

    /// Strong carrier sender for one attached session, so a newly opened
    /// stream can build an [`Outbox`].
    fn carrier_sender_for(&self, id: u64) -> Option<mpsc::Sender<Value>> {
        self.link
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .filter(|active| active.id == id)
            .map(|active| active.sender.clone())
    }

    /// Detach a session's broker: close its carrier channel so the transport
    /// write arm wakes and exits, and fail its streams. Used when a controller
    /// is replaced and its socket is already gone (normal drop happens when the
    /// transport loop itself ends).
    #[allow(dead_code)]
    pub(crate) fn detach_link(&self, id: u64) {
        let broker = {
            let mut guard = self
                .link
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            if guard.as_ref().is_some_and(|active| active.id == id) {
                guard.take().map(|active| active.broker)
            } else {
                None
            }
        };
        if let Some(broker) = broker {
            broker.fail_all(
                remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST,
                "hub link detached",
            );
        }
    }

    /// Shared HTTP client for direct-net forwarding and route probes.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Register a live per-instance listener.
    pub(crate) fn register_instance(&self, instance: Arc<listener::RelayInstance>) {
        self.instances
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(instance.instance_id(), instance);
    }

    /// Look up a live instance's listener (authentication/probe use). Kept
    /// for the task-2 Hub wiring; marked rather than deleted.
    #[allow(dead_code)]
    pub(crate) fn instance(&self, instance_id: &str) -> Option<Arc<listener::RelayInstance>> {
        self.instances
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(instance_id)
            .cloned()
    }

    /// Proxy side: install the gateway context for an instance the Hub routes
    /// here. Memory-only; nothing here is ever persisted.
    ///
    /// Production wiring arrives with the Hub-side relay session (api-routing
    /// task 2). It is a public entry point so integration tests can drive the
    /// egress without a Hub.
    #[allow(dead_code)]
    pub fn set_egress_context(&self, context: Arc<egress::EgressContext>) {
        self.egress
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(context.instance_id().to_owned(), context);
    }

    /// Look up an instance's proxy gateway context.
    pub(crate) fn egress_context(&self, instance_id: &str) -> Option<Arc<egress::EgressContext>> {
        self.egress
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(instance_id)
            .cloned()
    }

    /// Tear down everything one instance owned on this Node: its listener,
    /// its in-flight streams on the live link, and (on a proxy host) its
    /// gateway context. Idempotent; safe from build-failure, exit and purge.
    ///
    /// Synchronous so it can run from a worker-task termination guard: the
    /// graceful server shutdown completes asynchronously after the oneshot is
    /// taken, which is sufficient — the bearer is revoked immediately and new
    /// connections are refused.
    pub(crate) fn revoke_instance(&self, instance_id: &str) {
        let listener = self
            .instances
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(instance_id);
        if let Some(listener) = listener {
            listener.shutdown();
        }
        self.egress
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(instance_id);
        if let Some(link) = self.active_link() {
            link.cancel_instance(instance_id);
        }
    }
}

/// One byte chunk produced by reading an HTTP body, shared by the request
/// pump (W) and the egress (H).
pub(crate) type BodyChunk = Result<Bytes, std::io::Error>;

/// Revokes a freshly provisioned instance relay if the create never reaches a
/// running worker. The bearer is then dead exactly as it would be after an
/// instance exit, so a bind that succeeded but a write that failed cannot
/// leave a live listener behind. Disarmed once the worker owns the lifecycle.
pub(crate) struct ProvisionGuard {
    state: Arc<ApiRelayState>,
    instance_id: String,
    armed: bool,
}

impl std::fmt::Debug for ProvisionGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProvisionGuard")
            .field("instance_id", &self.instance_id)
            .field("armed", &self.armed)
            .finish()
    }
}

impl ProvisionGuard {
    pub(crate) fn new(state: Arc<ApiRelayState>, instance_id: String) -> Self {
        Self {
            state,
            instance_id,
            armed: true,
        }
    }

    /// Hand lifetime ownership to the spawned instance worker.
    pub(crate) fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for ProvisionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state.revoke_instance(&self.instance_id);
        }
    }
}

/// Values the launch overlay receives when an instance is served through the
/// relay: the loopback listener URL (plus base path) and the per-instance
/// bearer. These *replace* the gateway base URL and credential for everything
/// written into the 0600 settings file; the gateway values never reach W.
#[derive(Debug, Clone)]
pub(crate) struct RelayOverlay {
    /// `http://127.0.0.1:<port>` plus the profile's base path.
    pub base_url: String,
    /// Per-instance relay bearer.
    pub bearer: String,
}

/// Split body bytes into wire-sized chunks.
pub(crate) fn split_body_chunks(bytes: Bytes, chunk_bytes: usize) -> Vec<Bytes> {
    if bytes.len() <= chunk_bytes {
        return if bytes.is_empty() {
            Vec::new()
        } else {
            vec![bytes]
        };
    }
    let mut pieces = Vec::with_capacity(bytes.len().div_ceil(chunk_bytes));
    let mut rest = bytes;
    while rest.len() > chunk_bytes {
        pieces.push(rest.split_to(chunk_bytes));
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_chunk_size_and_order() {
        let chunks = split_body_chunks(Bytes::from(vec![0u8; 3 * 65_536 + 7]), 65_536);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].len(), 65_536);
        assert_eq!(chunks[3].len(), 7);
        assert!(split_body_chunks(Bytes::new(), 65_536).is_empty());
        assert_eq!(split_body_chunks(Bytes::from_static(b"x"), 65_536).len(), 1);
    }
}

#[cfg(test)]
mod relay_tests;
