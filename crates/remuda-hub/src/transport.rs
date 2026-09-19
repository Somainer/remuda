//! Hub↔Node RPC channels for outbound WSS and supervised SSH stdio.
//!
//! `ssh_hosts` owns the SSH process and attaches the same request/reply channel
//! used by the WebSocket dispatcher, with [`TransportKind::SshStdio`].

use crate::error::HubError;
use futures::Future;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};

/// How a Node is attached to this Hub process.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    /// Node dials `WS /v1/node` (production outbound WSS).
    OutboundWss,
    /// Hub (or an operator) runs `ssh <host> remuda node --stdio`.
    SshStdio,
}

impl TransportKind {
    /// Wire / SQLite spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutboundWss => "outbound-wss",
            Self::SshStdio => "ssh-stdio",
        }
    }

    /// Parse hello/heartbeat `transport` / `carrier` fields.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "outbound-wss" | "wss" => Some(Self::OutboundWss),
            "ssh-stdio" | "stdio" | "ssh" => Some(Self::SshStdio),
            _ => None,
        }
    }
}

/// JSON-RPC session to one enrolled Node, independent of the byte carrier.
pub trait NodeTransport: Send + Sync {
    /// Carrier advertised on the host registry.
    fn kind(&self) -> TransportKind;

    /// One request/response. `Ok(None)` means the session is not writable.
    fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>>;
}

/// Total in-flight RPCs one Node link accepts. Mirrors the protocol limit
/// (`TransportLimits::max_in_flight_rpc`); the Node never raises it per link.
pub(crate) const MAX_IN_FLIGHT_RPCS: usize = 32;

/// Bulk reads may occupy at most half the budget. The other half is kept for
/// control RPCs (`instance.create`, cancel, steer, writes, placement), so a
/// phone whose session list fans out one screen read per row can never close
/// the create path (2026-09-19 demo: the read fan-out refused `instance.create`
/// with an opaque INTERNAL).
pub(crate) const MAX_IN_FLIGHT_BULK_READS: usize = MAX_IN_FLIGHT_RPCS / 2;

/// Back-off hint carried on `NODE_BUSY`: one web poll cycle.
pub(crate) const NODE_BUSY_RETRY_AFTER_MS: u64 = 2_500;

/// Idempotent, side-effect-free *pulls* a UI may fan out (one call per listed
/// row or per drill-in). They draw from the bulk-read sub-budget
/// ([`MAX_IN_FLIGHT_BULK_READS`]) and fail retryable when saturated.
///
/// Everything else keeps the control reservation, including reads that are
/// part of a control flow (`host.resources` during placement,
/// `workspace.list`, `host.doctor`) and `tty.attach`, which opens a stream.
fn is_bulk_read(method: &str) -> bool {
    matches!(
        method,
        remuda_protocol::hubnode::METHOD_TTY_SCREEN
            | remuda_protocol::hubnode::METHOD_SUBAGENT_TRANSCRIPT
            | remuda_protocol::hubnode::METHOD_HOST_FILES_LIST
            | remuda_protocol::hubnode::METHOD_HOST_FILES_READ
            | remuda_protocol::hubnode::METHOD_HOST_FILES_SEARCH
    )
}

/// One waiting JSON-RPC caller. `bulk` marks bulk-read calls so they can be
/// capped without touching the control reservation.
pub(crate) struct PendingCall {
    pub(crate) tx: oneshot::Sender<Value>,
    bulk: bool,
}

/// Reply table shared by the socket pump and every [`WssTransport`] clone.
///
/// A plain (non-async) mutex by design: critical sections are one insert or
/// one remove, and [`PendingGuard`] must be able to evict its entry from a
/// synchronous `Drop` (a cancelled call future cannot await).
pub(crate) type PendingRpcs = Arc<StdMutex<HashMap<String, PendingCall>>>;

/// Create the reply table for one Node session.
#[must_use]
pub(crate) fn new_pending_rpcs() -> PendingRpcs {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// Fail every waiter on link teardown. Dropping the senders makes the calls
/// return `Ok(None)` ("session not writable") immediately instead of hanging
/// until their timeout, and guarantees no dead waiter outlives the socket.
pub(crate) fn fail_all_pending(pending: &PendingRpcs) {
    if let Ok(mut map) = pending.lock() {
        map.clear();
    }
}

/// Evicts one entry no matter how the call ends (reply, send failure, timeout,
/// cancellation, link drop). The table therefore cannot accumulate waiters.
struct PendingGuard {
    pending: PendingRpcs,
    rpc_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.rpc_id);
        }
    }
}

/// Live Node RPC channel, backed by a WebSocket or the SSH supervisor's writer.
#[derive(Clone)]
pub struct WssTransport {
    kind: TransportKind,
    outbound: mpsc::Sender<Value>,
    pending: PendingRpcs,
}

impl WssTransport {
    /// Pair with the WS writer task in `ws`.
    pub(crate) fn new(outbound: mpsc::Sender<Value>, pending: PendingRpcs) -> Self {
        Self {
            outbound,
            pending,
            kind: TransportKind::OutboundWss,
        }
    }
}

impl WssTransport {
    pub(crate) fn with_kind(mut self, kind: TransportKind) -> Self {
        self.kind = kind;
        self
    }
}

impl NodeTransport for WssTransport {
    fn kind(&self) -> TransportKind {
        self.kind
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        let method = method.to_string();
        Box::pin(async move {
            let rpc_id = uuid::Uuid::new_v4().to_string();
            let bulk = is_bulk_read(&method);
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let in_flight = pending.len();
                let bulk_in_flight = pending.values().filter(|call| call.bulk).count();
                if in_flight >= MAX_IN_FLIGHT_RPCS
                    || (bulk && bulk_in_flight >= MAX_IN_FLIGHT_BULK_READS)
                {
                    // Refused BEFORE the frame is queued, so no Node-side work
                    // happened: retryable, never INTERNAL. Control methods only
                    // reach this when the control half is itself full, which
                    // the read cap makes impossible under read fan-out.
                    return Err(HubError::NodeBusy {
                        retry_after_ms: NODE_BUSY_RETRY_AFTER_MS,
                    });
                }
                pending.insert(rpc_id.clone(), PendingCall { tx, bulk });
            }
            let frame = json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "method": method,
                "params": params,
            });
            // The guard owns removal from here on: every exit path below frees
            // exactly this slot, including a cancelled call future.
            let _guard = PendingGuard {
                pending: self.pending.clone(),
                rpc_id: rpc_id.clone(),
            };
            if self.outbound.send(frame).await.is_err() {
                return Ok(None);
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(value)) => Ok(Some(value)),
                // The session's reply table was dropped (link teardown): not a
                // Hub fault and not a Node answer — "not writable", same as a
                // failed outbound send.
                Ok(Err(_)) => Ok(None),
                Err(_) => {
                    if bulk {
                        // A slow screen row is retryable noise, not a 500.
                        Err(HubError::NodeBusy {
                            retry_after_ms: NODE_BUSY_RETRY_AFTER_MS,
                        })
                    } else {
                        Err(HubError::Internal("node rpc timeout".into()))
                    }
                }
            }
        })
    }
}

/// Legacy disconnected placeholder. Managed SSH uses the shared live RPC channel.
pub struct StdioTransport {
    host_id: String,
}

/// Scripted Node transport for Hub integration tests: every `call` returns a
/// fixed JSON-RPC frame (`None` models an unwritable session, i.e. 422).
///
/// It lets route tests exercise the proxy's 200/4xx/422 mapping without a real
/// WebSocket or a sleeping background task.
#[doc(hidden)]
pub struct ScriptedTransport {
    reply: Option<Value>,
    kind: TransportKind,
}

impl ScriptedTransport {
    /// Build a synthetic outbound-WSS node returning `reply` to every call.
    #[must_use]
    pub fn new(reply: Option<Value>) -> Self {
        Self {
            reply,
            kind: TransportKind::OutboundWss,
        }
    }
}

impl NodeTransport for ScriptedTransport {
    fn kind(&self) -> TransportKind {
        self.kind
    }

    fn call(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(self.reply.clone())))
    }
}

impl StdioTransport {
    /// Named host that will speak JSON-RPC on a stdio pipe once wired.
    pub fn pending(host_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
        }
    }

    /// Host this placeholder is reserved for.
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
}

impl NodeTransport for StdioTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::SshStdio
    }

    fn call(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }
}

struct NodeSlot {
    generation: u64,
    link: Arc<dyn NodeTransport>,
}

/// Connected Node transports, keyed by host id.
#[derive(Clone, Default)]
pub struct ConnectedNodes {
    inner: Arc<Mutex<HashMap<String, NodeSlot>>>,
}

impl ConnectedNodes {
    /// Call a JSON-RPC method on a connected Node. `Ok(None)` = not connected.
    pub async fn call(
        &self,
        host_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Option<Value>, HubError> {
        let Some(link) = self
            .inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.link.clone())
        else {
            return Ok(None);
        };
        link.call(method, params, timeout).await
    }

    /// Record a live session. Returns a generation used to retire only this session.
    pub async fn insert(&self, host_id: String, link: Arc<dyn NodeTransport>) -> u64 {
        let mut inner = self.inner.lock().await;
        let generation = inner
            .get(&host_id)
            .map(|slot| slot.generation.wrapping_add(1))
            .unwrap_or(1);
        inner.insert(host_id, NodeSlot { generation, link });
        generation
    }

    /// Drop the live session (does not kill the native Node process).
    pub async fn remove(&self, host_id: &str) {
        self.inner.lock().await.remove(host_id);
    }

    /// Drop the session only if `generation` is still current (stale WS must not offline a live host).
    pub async fn remove_generation(&self, host_id: &str, generation: u64) -> bool {
        let mut inner = self.inner.lock().await;
        match inner.get(host_id) {
            Some(slot) if slot.generation == generation => {
                inner.remove(host_id);
                true
            }
            _ => false,
        }
    }

    /// Carrier for a connected host, if any.
    pub async fn kind_of(&self, host_id: &str) -> Option<TransportKind> {
        self.inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.link.kind())
    }

    /// Host ids with a live Node session.
    pub async fn host_ids(&self) -> Vec<String> {
        self.inner.lock().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use remuda_protocol::hubnode::{
        METHOD_INSTANCE_CANCEL, METHOD_INSTANCE_CREATE, METHOD_TTY_SCREEN,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::task::JoinHandle;

    type CallResult = Result<Option<Value>, HubError>;

    /// Build a transport plus the socket side. The spawned pump answers
    /// control calls through the shared reply table and leaves every bulk read
    /// parked, modelling a Node slow to answer screen pulls. A frame carrying
    /// `"park": true` is parked regardless of method so the control-full case
    /// can be exercised too.
    fn harness() -> (WssTransport, PendingRpcs, Arc<AtomicUsize>) {
        let pending = new_pending_rpcs();
        let (outbound, mut inbound) = mpsc::channel::<Value>(64);
        let pump_pending = pending.clone();
        let answered = Arc::new(AtomicUsize::new(0));
        let pump_answered = answered.clone();
        tokio::spawn(async move {
            while let Some(frame) = inbound.recv().await {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                if is_bulk_read(method) || frame["params"]["park"] == true {
                    // Stay in flight: the model of a slow or saturated Node.
                    continue;
                }
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
                let waiter = pump_pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(id);
                if let Some(waiter) = waiter {
                    let _ = waiter.tx.send(reply);
                    pump_answered.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        (
            WssTransport::new(outbound, pending.clone()),
            pending,
            answered,
        )
    }

    fn pending_len(pending: &PendingRpcs) -> usize {
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Wait until the socket-side table holds exactly `want` waiters.
    async fn wait_pending(pending: &PendingRpcs, want: usize) {
        for _ in 0..200 {
            if pending_len(pending) == want {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(pending_len(pending), want, "waiters never parked");
    }

    /// Spawn a caller that stays parked (timeout never elapses in the test).
    fn spawn_parked(
        transport: WssTransport,
        method: &'static str,
        params: Value,
    ) -> JoinHandle<CallResult> {
        tokio::spawn(async move {
            transport
                .call(method, params, Duration::from_secs(3_600))
                .await
        })
    }

    /// 40 concurrent screen reads plus one instance.create: the create uses
    /// the reserved control capacity and succeeds; excess reads get the
    /// retryable NODE_BUSY and no call surfaces INTERNAL.
    #[tokio::test]
    async fn bulk_reads_cannot_starve_control() {
        let (transport, pending, answered) = harness();

        // Admitted reads park in the pump; the short timeout makes them settle
        // as the same retryable error the refused reads get immediately.
        let screens: Vec<JoinHandle<CallResult>> = (0..40)
            .map(|_| {
                let transport = transport.clone();
                tokio::spawn(async move {
                    transport
                        .call(METHOD_TTY_SCREEN, json!({}), Duration::from_millis(150))
                        .await
                })
            })
            .collect();

        // Worst-case ordering: every admissible read is parked before the
        // create claims a control slot.
        wait_pending(&pending, MAX_IN_FLIGHT_BULK_READS).await;
        let created = transport
            .call(METHOD_INSTANCE_CREATE, json!({}), Duration::from_secs(10))
            .await
            .expect("create must not error");
        assert_eq!(
            created.and_then(|v| v.pointer("/result/ok").cloned()),
            Some(json!(true))
        );
        assert_eq!(answered.load(Ordering::SeqCst), 1);

        for handle in screens {
            match handle.await.expect("screen task joins") {
                Err(HubError::NodeBusy { .. }) => {}
                other => panic!("expected NODE_BUSY, got {other:?}"),
            }
        }
        // Refusals and timeouts free every slot; the table cannot fill with
        // dead waiters.
        assert_eq!(pending_len(&pending), 0);
    }

    /// A dropped (cancelled) call future always removes its waiter.
    #[tokio::test]
    async fn cancelled_call_frees_its_slot() {
        let (transport, pending, _answered) = harness();
        let parked = spawn_parked(transport, "tty.attach", json!({ "park": true }));
        wait_pending(&pending, 1).await;
        parked.abort();
        assert!(parked.await.unwrap_err().is_cancelled());
        assert_eq!(pending_len(&pending), 0);
    }

    /// Socket teardown drains the table: waiters return "not writable" rather
    /// than hanging to their timeout.
    #[tokio::test]
    async fn link_drop_fails_all_waiters() {
        let (transport, pending, _answered) = harness();
        let parked = spawn_parked(transport, "tty.attach", json!({ "park": true }));
        wait_pending(&pending, 1).await;
        fail_all_pending(&pending);
        let result = tokio::time::timeout(Duration::from_secs(5), parked)
            .await
            .expect("caller finishes on teardown")
            .expect("task joins");
        assert!(matches!(result, Ok(None)), "{result:?}");
        assert_eq!(pending_len(&pending), 0);
    }

    /// Bulk admission is independent of control occupancy up to the
    /// reservation; even total saturation answers with the retryable error,
    /// never INTERNAL.
    #[tokio::test]
    async fn control_keeps_half_the_slots() {
        let (transport, pending, _answered) = harness();

        let mut parked = Vec::new();
        for _ in 0..MAX_IN_FLIGHT_BULK_READS {
            parked.push(spawn_parked(
                transport.clone(),
                METHOD_TTY_SCREEN,
                json!({}),
            ));
        }
        wait_pending(&pending, MAX_IN_FLIGHT_BULK_READS).await;

        // One read past the sub-budget is refused before sending a frame.
        let extra = transport
            .call(METHOD_TTY_SCREEN, json!({}), Duration::from_secs(60))
            .await;
        assert!(matches!(extra, Err(HubError::NodeBusy { .. })));

        // All 16 control slots remain usable.
        for _ in 0..(MAX_IN_FLIGHT_RPCS - MAX_IN_FLIGHT_BULK_READS) {
            let result = transport
                .call(METHOD_INSTANCE_CREATE, json!({}), Duration::from_secs(60))
                .await;
            assert!(matches!(result, Ok(Some(_))), "{result:?}");
        }

        // Now fill the control half with parked calls too.
        for _ in 0..(MAX_IN_FLIGHT_RPCS - MAX_IN_FLIGHT_BULK_READS) {
            parked.push(spawn_parked(
                transport.clone(),
                METHOD_INSTANCE_CANCEL,
                json!({ "park": true }),
            ));
        }
        wait_pending(&pending, MAX_IN_FLIGHT_RPCS).await;

        // 33rd call: control half full as well — still the retryable error.
        let overflow = transport
            .call(METHOD_INSTANCE_CREATE, json!({}), Duration::from_secs(60))
            .await;
        assert!(matches!(overflow, Err(HubError::NodeBusy { .. })));

        for handle in parked {
            handle.abort();
            let _ = handle.await;
        }
        assert_eq!(pending_len(&pending), 0);
    }
}
