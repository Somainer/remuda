//! Pull staged object bytes over the live Hub↔Node carrier (`object.pull`).
//!
//! A host enrolled over the ssh-stdio bridge generally has no HTTP route to
//! the Hub: the bridge is its only channel. [`CarrierObjectBroker`] speaks the
//! Node→Hub `object.pull` request on that channel, correlates the Hub's reply,
//! and reassembles any preceding `object.chunk` notifications. One broker per
//! live connection is shared by every instance worker; the transport loops
//! (stdio NDJSON and outbound WSS) feed inbound frames to
//! [`CarrierObjectBroker::handle_frame`] and drain its outbound frames into
//! the socket.
//!
//! Delivery contract:
//! - one in-flight pull per object id (duplicate concurrent callers fail fast);
//! - a bounded per-attempt deadline, with one retry on transport-class loss;
//! - size and SHA-256 from the reply are verified after reassembly, so a
//!   truncated or interleaved transfer can never be materialized.

use crate::NodeError;
use crate::attachments::ObjectSource;
use base64::Engine;
use remuda_protocol::InstanceId;
use remuda_protocol::hubnode::METHOD_OBJECT_PULL;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Deadline for one pull attempt. A send waits on the result, so this stays
/// short enough to surface a dead carrier but long enough for a 25 MiB object
/// over a slow SSH link.
pub(crate) const PULL_TIMEOUT: Duration = Duration::from_secs(90);
/// How often the deadline table is swept.
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);

type PullReply = oneshot::Sender<Result<Vec<u8>, NodeError>>;
type PullReceiver = oneshot::Receiver<Result<Vec<u8>, NodeError>>;

struct PendingPull {
    object_id: String,
    reply: Option<PullReply>,
    chunks: BTreeMap<u32, Vec<u8>>,
    /// Sequence of the chunk carrying `last: true`, once it has arrived.
    last_seq: Option<u32>,
    deadline: Instant,
}

struct BrokerState {
    /// Rpc id -> waiter.
    pending: HashMap<String, PendingPull>,
    /// Object id -> rpc id, so `object.chunk` notifications (keyed by object)
    /// find their transfer.
    objects: HashMap<String, String>,
    /// Object ids with an active transfer; enforces one in-flight per object.
    in_flight: HashSet<String>,
}

/// Correlates `object.pull` requests with their replies on one connection.
pub(crate) struct CarrierObjectBroker {
    /// Frames to write onto the carrier.
    outbound: mpsc::Sender<Value>,
    next_id: AtomicU64,
    /// Per-attempt deadline.
    timeout: Duration,
    state: Mutex<BrokerState>,
}

impl std::fmt::Debug for CarrierObjectBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarrierObjectBroker")
            .finish_non_exhaustive()
    }
}

impl CarrierObjectBroker {
    /// Build a broker writing request frames onto `outbound` and start its
    /// deadline sweeper. The sweeper holds only a [`Weak`], so it stops when
    /// the connection drops the broker even if a fetch future was leaked.
    pub(crate) fn new(outbound: mpsc::Sender<Value>) -> Arc<Self> {
        Self::new_with_timeout(outbound, PULL_TIMEOUT)
    }

    /// Build a broker with a custom per-pull deadline (tests).
    #[cfg(test)]
    pub(crate) fn new_for_test(outbound: mpsc::Sender<Value>, timeout: Duration) -> Arc<Self> {
        Self::new_with_timeout(outbound, timeout)
    }

    fn new_with_timeout(outbound: mpsc::Sender<Value>, timeout: Duration) -> Arc<Self> {
        let broker = Arc::new(Self {
            outbound,
            next_id: AtomicU64::new(1),
            timeout,
            state: Mutex::new(BrokerState {
                pending: HashMap::new(),
                objects: HashMap::new(),
                in_flight: HashSet::new(),
            }),
        });
        let weak = Arc::downgrade(&broker);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                let Some(broker) = weak.upgrade() else { return };
                broker.sweep();
            }
        });
        broker
    }

    /// An [`ObjectSource`] bound to this connection.
    pub(crate) fn source(self: &Arc<Self>) -> CarrierObjectSource {
        CarrierObjectSource {
            broker: Arc::clone(self),
        }
    }

    /// Fail every in-flight pull (connection torn down / reconnecting). The
    /// [`CarrierObjectSource`] retry decides whether a fresh attempt rides the
    /// next socket; stale replies arriving afterwards match nothing.
    pub(crate) fn fail_all(&self, error: NodeError) {
        let mut pulled = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.objects.clear();
            state.in_flight.clear();
            for (_, mut pending) in state.pending.drain() {
                if let Some(reply) = pending.reply.take() {
                    pulled.push(reply);
                }
            }
        }
        for reply in pulled {
            let _ = reply.send(Err(clone_error(&error)));
        }
    }

    /// Register and send one `object.pull`. Returns the rpc id and a receiver
    /// for the reassembled bytes.
    fn begin(
        self: &Arc<Self>,
        object_id: String,
        instance_id: &InstanceId,
    ) -> Result<(String, PullReceiver), NodeError> {
        let rpc_id = format!("objpull-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (reply, receiver) = oneshot::channel();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !state.in_flight.insert(object_id.clone()) {
                return Err(NodeError::InvalidRequest(format!(
                    "object {object_id} already has an in-flight pull"
                )));
            }
            state.objects.insert(object_id.clone(), rpc_id.clone());
            state.pending.insert(
                rpc_id.clone(),
                PendingPull {
                    object_id: object_id.clone(),
                    reply: Some(reply),
                    chunks: BTreeMap::new(),
                    last_seq: None,
                    deadline: Instant::now() + self.timeout,
                },
            );
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": METHOD_OBJECT_PULL,
            "params": {
                "objectId": object_id,
                "instanceId": instance_id.as_id().as_str(),
            },
        });
        // Roll the registration back if the carrier writer is already closed.
        if self.outbound.try_send(frame).is_err() {
            self.cancel(&rpc_id);
            return Err(NodeError::Disconnected);
        }
        Ok((rpc_id, receiver))
    }

    /// Drop a transfer the caller no longer awaits (its deadline elapsed).
    fn cancel(&self, rpc_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(pending) = state.pending.remove(rpc_id) {
            state.objects.remove(&pending.object_id);
            state.in_flight.remove(&pending.object_id);
        }
    }

    /// Feed one inbound carrier frame. Returns true when the broker consumed
    /// it (a `object.pull` reply or an `object.chunk` notification); unknown
    /// ids and unrelated frames return false for the transport's normal
    /// routing.
    pub(crate) fn handle_frame(&self, frame: &Value) -> bool {
        match frame.get("method").and_then(Value::as_str) {
            Some("object.chunk") => {
                self.handle_chunk(frame.get("params").unwrap_or(&Value::Null));
                // The notification was addressed to this facility; even an
                // unmatched object id is consumed rather than RPC-rejected.
                true
            }
            None => self.handle_reply(frame),
            _ => false,
        }
    }

    fn handle_chunk(&self, params: &Value) {
        let Some(object_id) = params.get("objectId").and_then(Value::as_str) else {
            return;
        };
        let Some(seq) = params.get("seq").and_then(Value::as_u64) else {
            return;
        };
        let Some(data_base64) = params.get("dataBase64").and_then(Value::as_str) else {
            return;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_base64.as_bytes())
        else {
            tracing::warn!(%object_id, seq, "object.chunk carried undecodable base64");
            return;
        };
        let seq = match u32::try_from(seq) {
            Ok(seq) => seq,
            Err(_) => return,
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(rpc_id) = state.objects.get(object_id) else {
            tracing::debug!(%object_id, seq, "object.chunk for an unknown transfer; ignored");
            return;
        };
        let rpc_id = rpc_id.clone();
        let Some(pending) = state.pending.get_mut(&rpc_id) else {
            return;
        };
        pending.chunks.insert(seq, bytes);
        if params.get("last").and_then(Value::as_bool).unwrap_or(false) {
            pending.last_seq = Some(seq);
        }
    }

    fn handle_reply(&self, frame: &Value) -> bool {
        let Some(id) = frame.get("id").and_then(|id| {
            id.as_str()
                .map(str::to_owned)
                .or_else(|| id.as_u64().map(|n| n.to_string()))
        }) else {
            return false;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.pending.contains_key(&id) {
            return false;
        }
        let mut pending = state.pending.remove(&id).unwrap();
        state.objects.remove(&pending.object_id);
        state.in_flight.remove(&pending.object_id);
        drop(state);

        let outcome = Self::outcome_from_reply(&mut pending, frame);
        if let Some(reply) = pending.reply.take() {
            let _ = reply.send(outcome);
        }
        true
    }

    fn outcome_from_reply(pending: &mut PendingPull, frame: &Value) -> Result<Vec<u8>, NodeError> {
        if let Some(error) = frame.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("hub error")
                .to_owned();
            return Err(NodeError::HubRpc { code, message });
        }
        let result = frame
            .get("result")
            .ok_or_else(|| NodeError::Transport("object.pull reply missing result".into()))?;
        if let Some(reply_object) = result
            .get("objectId")
            .and_then(Value::as_str)
            .filter(|value| *value != pending.object_id)
        {
            return Err(NodeError::Transport(format!(
                "object.pull reply named {reply_object}, expected {}",
                pending.object_id
            )));
        }
        let bytes = match result.get("dataBase64").and_then(Value::as_str) {
            Some(data_base64) => base64::engine::general_purpose::STANDARD
                .decode(data_base64.as_bytes())
                .map_err(|error| {
                    NodeError::Transport(format!("object.pull inline base64 invalid: {error}"))
                })?,
            None => assemble_chunks(pending)?,
        };
        if let Some(size) = result.get("size").and_then(Value::as_u64)
            && size as usize != bytes.len()
        {
            return Err(NodeError::InvalidRequest(format!(
                "object {} transfer size mismatch: got {} bytes, reply said {size}",
                pending.object_id,
                bytes.len()
            )));
        }
        if let Some(sha) = result.get("sha256").and_then(Value::as_str) {
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if actual != sha {
                return Err(NodeError::InvalidRequest(format!(
                    "object {} failed integrity check: digest {actual} != reply {sha}",
                    pending.object_id
                )));
            }
        }
        Ok(bytes)
    }

    fn sweep(&self) {
        let now = Instant::now();
        let mut expired = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let due: Vec<String> = state
                .pending
                .iter()
                .filter(|(_, pending)| pending.deadline <= now)
                .map(|(id, _)| id.clone())
                .collect();
            for id in due {
                if let Some(mut pending) = state.pending.remove(&id) {
                    state.objects.remove(&pending.object_id);
                    state.in_flight.remove(&pending.object_id);
                    if let Some(reply) = pending.reply.take() {
                        expired.push(reply);
                    }
                }
            }
        }
        for reply in expired {
            let _ = reply.send(Err(NodeError::Transport(
                "object.pull deadline elapsed without a complete reply".into(),
            )));
        }
    }
}

/// Concatenate received chunks, requiring the contiguous range `0..=last`.
fn assemble_chunks(pending: &PendingPull) -> Result<Vec<u8>, NodeError> {
    let last = pending.last_seq.ok_or_else(|| {
        NodeError::Transport(format!(
            "object {} transfer ended before the final object.chunk",
            pending.object_id
        ))
    })?;
    let mut out = Vec::new();
    for seq in 0..=last {
        match pending.chunks.get(&seq) {
            Some(chunk) => out.extend_from_slice(chunk),
            None => {
                return Err(NodeError::Transport(format!(
                    "object {} transfer is missing chunk {seq} of {last}",
                    pending.object_id
                )));
            }
        }
    }
    Ok(out)
}

/// Errors move through a oneshot and a fail-all fanout, so clone the handful of
/// transport-class variants explicitly rather than forcing `Clone` on the whole
/// error enum.
fn clone_error(error: &NodeError) -> NodeError {
    match error {
        NodeError::Transport(message) => NodeError::Transport(message.clone()),
        other => NodeError::Transport(other.to_string()),
    }
}

/// [`ObjectSource`] that pulls over the active carrier.
#[derive(Clone)]
pub(crate) struct CarrierObjectSource {
    broker: Arc<CarrierObjectBroker>,
}

impl std::fmt::Debug for CarrierObjectSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarrierObjectSource")
            .finish_non_exhaustive()
    }
}

impl CarrierObjectSource {
    /// One bounded pull attempt.
    async fn attempt(
        &self,
        object_id: String,
        instance_id: InstanceId,
    ) -> Result<Vec<u8>, NodeError> {
        let (rpc_id, receiver) = self.broker.clone().begin(object_id, &instance_id)?;
        match tokio::time::timeout(PULL_TIMEOUT, receiver).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => Err(NodeError::Disconnected),
            Err(_) => {
                self.broker.cancel(&rpc_id);
                Err(NodeError::Transport(
                    "object.pull timed out waiting for the Hub".into(),
                ))
            }
        }
    }

    /// Whether a failure is worth one immediate retry over a (possibly
    /// reconnected) carrier. Application-level refusals are not.
    fn is_retriable(error: &NodeError) -> bool {
        matches!(
            error,
            NodeError::Disconnected | NodeError::Transport(_) | NodeError::JournalQueueClosed
        )
    }
}

impl ObjectSource for CarrierObjectSource {
    fn fetch_for_instance(
        &self,
        object_id: String,
        instance_id: &InstanceId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>>
    {
        let source = Self {
            broker: self.broker.clone(),
        };
        let instance_id = instance_id.clone();
        Box::pin(async move {
            let mut last_error = None;
            for attempt in 0..2 {
                match source.attempt(object_id.clone(), instance_id.clone()).await {
                    Ok(bytes) => return Ok(bytes),
                    // One immediate retry, on a fresh pull, for transport loss
                    // (deadline, closed socket). A refused/unknown object is a
                    // terminal answer and is not retried.
                    Err(error) if attempt == 0 && Self::is_retriable(&error) => {
                        tracing::debug!(
                            %error,
                            "object.pull transport attempt failed; retrying once"
                        );
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| NodeError::Transport("object.pull failed".into())))
        })
    }
}

/// HTTP-first object source that sticks to the carrier for the rest of a
/// connection once the Hub HTTP origin proves unreachable.
///
/// Used by the outbound-WSS runtime, which normally has direct HTTP access but
/// may be enrolled behind the same NAT as a stdio host. The unreachable bit is
/// (re)initialized per connection, so a reconnect re-probes HTTP once.
#[derive(Debug)]
pub(crate) struct FallbackObjectSource {
    http: crate::attachments::HubObjectSource,
    carrier: CarrierObjectSource,
    http_unreachable: AtomicBool,
}

impl FallbackObjectSource {
    pub(crate) fn new(
        http: crate::attachments::HubObjectSource,
        carrier: CarrierObjectSource,
    ) -> Self {
        Self {
            http,
            carrier,
            http_unreachable: AtomicBool::new(false),
        }
    }
}

impl ObjectSource for FallbackObjectSource {
    fn fetch_for_instance(
        &self,
        object_id: String,
        instance_id: &InstanceId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, NodeError>> + Send + '_>>
    {
        let instance_id = instance_id.clone();
        Box::pin(async move {
            if self.http_unreachable.load(Ordering::SeqCst) {
                return self
                    .carrier
                    .fetch_for_instance(object_id, &instance_id)
                    .await;
            }
            match self.http.fetch(object_id.clone()).await {
                Ok(bytes) => Ok(bytes),
                Err(error) if CarrierObjectSource::is_retriable(&error) => {
                    // One probe per connection: a failed dial marks HTTP down
                    // and every later pull on this socket uses the carrier.
                    self.http_unreachable.store(true, Ordering::SeqCst);
                    tracing::info!(
                        %error,
                        "Hub HTTP origin unreachable; pulling objects over the carrier"
                    );
                    self.carrier
                        .fetch_for_instance(object_id, &instance_id)
                        .await
                }
                Err(error) => Err(error),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a broker against a scripted peer: outbound object.pull frames are
    /// captured and replies/chunks are fed back in.
    struct Harness {
        broker: Arc<CarrierObjectBroker>,
        outbound: mpsc::Receiver<Value>,
        instance: InstanceId,
    }

    impl Harness {
        /// Pull deadline kept short so the timeout test runs in real time
        /// without tokio's `test-util` clock.
        const TEST_TIMEOUT: Duration = Duration::from_millis(250);

        fn new() -> Self {
            let (tx, rx) = mpsc::channel(8);
            Self {
                broker: CarrierObjectBroker::new_for_test(tx, Self::TEST_TIMEOUT),
                outbound: rx,
                instance: InstanceId::new(),
            }
        }

        /// Start one attempt in the background and return the request frame it
        /// emitted, so the test can script the reply.
        async fn start_pull(
            &mut self,
            object_id: &str,
        ) -> (tokio::task::JoinHandle<Result<Vec<u8>, NodeError>>, Value) {
            let broker = self.broker.clone();
            let instance = self.instance.clone();
            let object_id = object_id.to_owned();
            let expected_id = object_id.clone();
            let pull =
                tokio::spawn(async move { broker.source().attempt(object_id, instance).await });
            let request = self.outbound.recv().await.expect("object.pull frame");
            assert_eq!(request["method"], METHOD_OBJECT_PULL);
            assert_eq!(request["params"]["objectId"], expected_id);
            assert_eq!(
                request["params"]["instanceId"],
                self.instance.as_id().as_str()
            );
            (pull, request)
        }

        fn reply(&self, value: Value) {
            assert!(self.broker.handle_frame(&value));
        }

        fn chunk(&self, object_id: &str, seq: u32, bytes: &[u8], last: bool) {
            let frame = json!({
                "jsonrpc": "2.0",
                "method": "object.chunk",
                "params": {
                    "objectId": object_id,
                    "seq": seq,
                    "last": last,
                    "dataBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
                },
            });
            assert!(self.broker.handle_frame(&frame));
        }
    }

    #[tokio::test]
    async fn small_object_returns_inline() {
        let mut harness = Harness::new();
        let bytes = b"hello brief\n".to_vec();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let pull = tokio::spawn({
            let instance = harness.instance.clone();
            let broker = harness.broker.clone();
            async move { broker.source().attempt("obj_small".into(), instance).await }
        });
        let request = harness.outbound.recv().await.unwrap();
        let id = request["id"].clone();
        harness.reply(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "objectId": "obj_small",
                "mime": "text/plain",
                "size": bytes.len(),
                "sha256": digest,
                "dataBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            },
        }));
        assert_eq!(pull.await.unwrap().unwrap(), bytes);
    }

    #[tokio::test]
    async fn chunked_object_is_reassembled_in_order() {
        let mut harness = Harness::new();
        let bytes: Vec<u8> = (0..100_000u32).map(|n| (n % 251) as u8).collect();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let pull = tokio::spawn({
            let instance = harness.instance.clone();
            let broker = harness.broker.clone();
            async move { broker.source().attempt("obj_big".into(), instance).await }
        });
        let request = harness.outbound.recv().await.unwrap();
        let id = request["id"].clone();
        // Deliver chunks out of order; the broker must sequence them.
        let chunks: Vec<_> = bytes.chunks(30_000).collect();
        let last_index = (chunks.len() - 1) as u32;
        harness.chunk("obj_big", 1, chunks[1], false);
        harness.chunk("obj_big", 0, chunks[0], false);
        harness.chunk("obj_big", 2, chunks[2], 2 == last_index);
        if last_index > 2 {
            harness.chunk("obj_big", last_index, chunks[last_index as usize], true);
        }
        harness.reply(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "objectId": "obj_big",
                "mime": "application/octet-stream",
                "size": bytes.len(),
                "sha256": digest,
            },
        }));
        assert_eq!(pull.await.unwrap().unwrap(), bytes);
    }

    #[tokio::test]
    async fn sha_mismatch_is_rejected() {
        let mut harness = Harness::new();
        let (pull, request) = harness.start_pull("obj_bad").await;
        harness.reply(json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "objectId": "obj_bad",
                "mime": "text/plain",
                "size": 5,
                "sha256": "0".repeat(64),
                "dataBase64": base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        }));
        let error = pull.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("integrity check"), "{error}");
    }

    #[tokio::test]
    async fn missing_reply_times_out() {
        // The broker's sweeper fails the waiter once the deadline passes, with
        // no reply on the wire. The source retries once, so bound the whole
        // thing over the two short test deadlines.
        let mut harness = Harness::new();
        let (pull, request) = harness.start_pull("obj_lost").await;
        assert_eq!(request["params"]["objectId"], "obj_lost");

        let error = tokio::time::timeout(Duration::from_secs(5), pull)
            .await
            .expect("test guard")
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(&error, NodeError::Transport(message) if message.contains("deadline")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn hub_error_is_surfaced_and_unrelated_frames_pass_through() {
        let mut harness = Harness::new();
        let (pull, request) = harness.start_pull("obj_nope").await;
        harness.reply(json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "error": { "code": -32601, "message": "not found" },
        }));
        let error = pull.await.unwrap().unwrap_err();
        assert!(matches!(error, NodeError::HubRpc { code: -32601, .. }));

        // Frames for other facilities are not consumed.
        assert!(!harness.broker.handle_frame(&json!({
            "jsonrpc": "2.0", "id": "journal-x", "result": { "seq": "1" }
        })));
        assert!(!harness.broker.handle_frame(&json!({
            "jsonrpc": "2.0", "method": "tty.mode", "params": {}
        })));
    }
}
