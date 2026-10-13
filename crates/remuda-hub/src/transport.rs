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
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
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

    /// Send a JSON-RPC **notification** (no `id`).
    ///
    /// D-048's `api.*` frames are notifications by design: they must never
    /// enter the 32-slot in-flight request map that `instance.create` and
    /// `tty.write` depend on. The frame rides the same FIFO outbound channel
    /// (so it cannot overtake queued RPC replies) but never registers a waiter.
    /// Returns `Ok(false)` when the session is not writable.
    fn notify(
        &self,
        method: &str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>>;
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

/// Fail every waiter on link teardown. Dropping the senders makes calls
/// return immediately (a lost-reply Err for frames already sent — callers
/// reconcile those rather than claim they never ran — see the `call` match),
/// and guarantees no dead waiter outlives the socket.
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
                // Pre-send failure: the Node never saw the frame. Callers read
                // Ok(None) as "not connected / not writable" and leave queued
                // commands resting, not executing.
                return Ok(None);
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(value)) => Ok(Some(value)),
                // The frame was queued and the reply table was then dropped
                // (link teardown). The Node may have run the command, so this
                // must stay an Err that command forwarding treats like a lost
                // reply (mark_reconciling); Ok(None) would falsely mean
                // "not connected" and settle the row as never sent.
                Ok(Err(_)) => Err(HubError::Internal("node rpc dropped".into())),
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

    fn notify(
        &self,
        method: &str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>> {
        let method = method.to_string();
        Box::pin(async move {
            let frame = json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            });
            match self.outbound.send(frame).await {
                Ok(()) => Ok(true),
                Err(_) => Ok(false),
            }
        })
    }
}

/// Legacy disconnected placeholder. Managed SSH uses the shared live RPC channel.
pub struct StdioTransport {
    host_id: String,
}

/// Gate for a [`GatedTransport`] call: the transport blocks every RPC until the
/// test opens the gate, then reports the frame was never queued (`Ok(None)`).
/// Lets a test hold a forward attempt inside its mark-intent → rollback window
/// while it exercises command GETs bound to that attempt.
#[doc(hidden)]
#[derive(Clone)]
pub struct CallGate {
    open: Arc<tokio::sync::Mutex<bool>>,
    notify: Arc<tokio::sync::Notify>,
}

impl CallGate {
    /// Create a closed gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: Arc::new(tokio::sync::Mutex::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Block until the gate is open (returns immediately once it is).
    async fn wait(&self) {
        loop {
            if *self.open.lock().await {
                return;
            }
            let notified = self.notify.notified();
            if *self.open.lock().await {
                return;
            }
            notified.await;
        }
    }

    /// Open the gate and release every blocked call.
    pub async fn open_gate(&self) {
        *self.open.lock().await = true;
        self.notify.notify_waiters();
    }
}

impl Default for CallGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Scripted Node transport that blocks each `call` on a [`CallGate`], then
/// answers `Ok(None)` ("frame never queued"), modelling a live-looking link
/// that refuses the frame before queueing — while a test holds the forward
/// attempt open.
#[doc(hidden)]
pub struct GatedTransport {
    gate: CallGate,
    kind: TransportKind,
}

impl GatedTransport {
    /// Build an outbound-WSS transport blocked on `gate`.
    #[must_use]
    pub fn new(gate: CallGate) -> Self {
        Self {
            gate,
            kind: TransportKind::OutboundWss,
        }
    }
}

impl NodeTransport for GatedTransport {
    fn kind(&self) -> TransportKind {
        self.kind
    }

    fn call(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        let gate = self.gate.clone();
        Box::pin(async move {
            gate.wait().await;
            // The frame never queued on the Node.
            Ok(None)
        })
    }

    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
    }
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

    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
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

    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

struct NodeSlot {
    generation: u64,
    link: Arc<dyn NodeTransport>,
}

/// Result of registering a carrier session.
pub(crate) struct InsertedSession {
    /// The NON-REUSED authenticated session identity minted for this socket.
    /// A new, globally-unique value is handed out on every registration, even
    /// when the host had fully disappeared in between (so it can never equal a
    /// superseded socket's identity).
    pub(crate) generation: u64,
    /// True when an older LIVE socket still occupied the slot at insert — a
    /// socket-level takeover rather than a reconnect after a full disconnect.
    /// The detached post-hello Landing decision uses this (under the per-host
    /// guard) instead of re-querying the map, so an old socket tearing down
    /// after the hello cannot change the answer (r16 item 6).
    pub(crate) displaced_live: bool,
}

/// Connected Node transports, keyed by host id.
#[derive(Clone)]
pub struct ConnectedNodes {
    inner: Arc<Mutex<HashMap<String, NodeSlot>>>,
    /// Per-host authority lock. A Landing settlement/evidence/grant WRITER
    /// takes a shared (read) guard from its current-session pre-check through
    /// its store commit; a session replacement (hello insert / socket teardown)
    /// takes the exclusive (write) guard around changing the host's live
    /// session. They therefore serialize: either the writer commits while its
    /// session is still current, or the replacement completes first and the
    /// writer's commit predicate matches nothing (r15 item 3, R2).
    authority: Arc<Mutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>>,
    /// Hosts whose CURRENT session was installed while an older socket was
    /// still live (a socket-level takeover). Maps host → the TAKEOVER
    /// generation (the newer session). It is cleared only when THAT
    /// generation is removed under the per-host guard; a stale OLDER socket
    /// tearing down later must never clear it (r16 item 6). It denies the new
    /// socket inheritance of the old session's Landing grants (a genuine
    /// reconnect after a full disconnect leaves the map empty).
    live_takeover: Arc<Mutex<HashMap<String, u64>>>,
    /// Test-only: notified once per host when a session replacement (insert)
    /// has to WAIT for an in-flight Landing authority decision to commit — the
    /// deterministic witness that takeover and verdict-writer are serialized
    /// (r15 item 3).
    #[cfg(any(test, feature = "test-faults"))]
    authority_contention: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// Boot epoch minted in the Hub DB and incremented at EVERY Hub boot. A
    /// session identity is `(boot_epoch << 32) | per-boot counter`, so an id
    /// handed out by a previous Hub process can NEVER be minted again after a
    /// restart (r14 R3): the in-memory counter resetting to 1 used to hand a
    /// new socket the same identity a superseded socket persisted on a Landing.
    boot_epoch: u64,
    next_session: Arc<AtomicU32>,
}

impl ConnectedNodes {
    /// Boot identity: allocate the next persisted boot epoch (monotonic across
    /// Hub restarts on the same data dir) and start this process's per-boot
    /// session counter at zero.
    ///
    /// r15 item 6 — fail closed: a missing/regressed epoch or a DB allocation
    /// failure returns an error so the Hub refuses to start instead of minting
    /// possibly-repeated session ids with a silently reset epoch.
    pub(crate) async fn boot(
        store: &crate::store::Store,
    ) -> Result<Self, crate::store::StoreError> {
        let boot_epoch = store.next_gate_session_boot_epoch().await?;
        tracing::info!(boot_epoch, "gate carrier session boot epoch allocated");
        Ok(Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            authority: Arc::new(Mutex::new(HashMap::new())),
            live_takeover: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(any(test, feature = "test-faults"))]
            authority_contention: Arc::new(Mutex::new(HashMap::new())),
            boot_epoch,
            next_session: Arc::new(AtomicU32::new(0)),
        })
    }

    /// This Hub process's boot epoch (high 32 bits of every session id).
    pub(crate) fn boot_epoch(&self) -> u64 {
        self.boot_epoch
    }

    /// Test-only: an empty registry with a fixed boot epoch.
    #[cfg(test)]
    fn for_tests(boot_epoch: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            authority: Arc::new(Mutex::new(HashMap::new())),
            live_takeover: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(any(test, feature = "test-faults"))]
            authority_contention: Arc::new(Mutex::new(HashMap::new())),
            boot_epoch,
            next_session: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The per-host authority lock (r15 item 3). Shared guards pin the live
    /// session across a writer's pre-check → commit stretch; the exclusive
    /// guard is held while a hello/takedown replaces the session.
    pub(crate) async fn host_authority(&self, host_id: &str) -> Arc<tokio::sync::RwLock<()>> {
        let mut authority = self.authority.lock().await;
        authority
            .entry(host_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    /// True when the host's current session was installed over an older LIVE
    /// socket AND `generation` is exactly that takeover generation — the check
    /// a post-hello Landing decision makes under the per-host guard.
    pub(crate) async fn host_had_live_takeover(&self, host_id: &str, generation: u64) -> bool {
        self.live_takeover.lock().await.get(host_id) == Some(&generation)
    }

    /// Clear the takeover mark by GENERATION under the already-held per-host
    /// authority guard. Only the removal of the matching CURRENT session clears
    /// it: a stale OLDER socket tearing down after a newer hello must not wipe
    /// the mark (r16 item 6). Returns true when the mark was cleared here.
    pub(crate) async fn clear_live_takeover_generation_locked(
        &self,
        host_id: &str,
        generation: u64,
    ) -> bool {
        let mut live_takeover = self.live_takeover.lock().await;
        if live_takeover.get(host_id) == Some(&generation) {
            live_takeover.remove(host_id);
            true
        } else {
            false
        }
    }

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

    /// Send a notification (no `id`, no pending slot) to a connected Node.
    /// `Ok(false)` = not connected / session not writable.
    pub async fn notify(
        &self,
        host_id: &str,
        method: &str,
        params: Value,
    ) -> Result<bool, HubError> {
        let Some(link) = self
            .inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.link.clone())
        else {
            return Ok(false);
        };
        link.notify(method, params).await
    }

    /// Record a live session. Mints a fresh, never-reused session identity
    /// (process-wide monotonic) and reports whether it superseded a still-live
    /// session for the same host. The returned generation retires only this
    /// session in [`Self::remove_generation`].
    pub(crate) async fn insert(
        &self,
        host_id: String,
        link: Arc<dyn NodeTransport>,
    ) -> InsertedSession {
        // Exclusive with every in-flight Landing authority decision for this
        // host (r15 item 3): a writer that passed its current-session pre-check
        // must commit before this socket becomes current, and vice versa.
        let lock = self.host_authority(host_id.as_str()).await;
        // Fast path under no contention; when a reader (a parked settlement)
        // holds the guard, notify test waiters BEFORE awaiting.
        if let Ok(guard) = lock.try_write() {
            let _authority = guard;
            return self.insert_locked(host_id, link).await;
        }
        #[cfg(any(test, feature = "test-faults"))]
        {
            let notify = self.authority_write_notify(host_id.as_str()).await;
            notify.notify_waiters();
        }
        let _authority = lock.write_owned().await;
        self.insert_locked(host_id, link).await
    }

    /// Register the new session with the authority guard already held.
    async fn insert_locked(
        &self,
        host_id: String,
        link: Arc<dyn NodeTransport>,
    ) -> InsertedSession {
        let mut inner = self.inner.lock().await;
        let counter = self.next_session.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        // Pack the persisted boot epoch and this boot's monotonic counter so the
        // id is unique across Hub restarts as well as within one boot (r14 R3).
        let generation = self.boot_epoch << 32 | u64::from(counter);
        // A registration while another live session occupies the slot is a
        // socket-level takeover (the old socket has not torn down). Record it
        // so post-hello Landing handling does not rebind A's grants to B; a
        // registration into an empty slot (genuine reconnect) leaves it clear.
        let displaced_live = inner.contains_key(&host_id);
        inner.insert(host_id.clone(), NodeSlot { generation, link });
        if displaced_live {
            // Keyed by the NEW (takeover) generation; only that session's
            // guarded removal clears it (r16 item 6).
            self.live_takeover.lock().await.insert(host_id, generation);
        }
        InsertedSession {
            generation,
            displaced_live,
        }
    }

    /// Test-only: latch for "an insert is waiting on this host's authority
    /// guard".
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn authority_write_notify(&self, host_id: &str) -> Arc<tokio::sync::Notify> {
        let mut contention = self.authority_contention.lock().await;
        contention
            .entry(host_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    /// True iff `session` is the CURRENT live session registered for this host.
    /// A superseded-but-still-open socket holds a stale generation and fails
    /// this check even though it remains connected at the TCP layer.
    pub async fn is_current_session(&self, host_id: &str, session: u64) -> bool {
        self.inner
            .lock()
            .await
            .get(host_id)
            .is_some_and(|slot| slot.generation == session)
    }

    /// Drop the live session (does not kill the native Node process).
    pub async fn remove(&self, host_id: &str) {
        // Exclusive with in-flight authority decisions, like insert (r15 item 3).
        let _authority = self.host_authority(host_id).await.write_owned().await;
        let removed_generation = {
            let mut inner = self.inner.lock().await;
            let generation = inner.get(host_id).map(|slot| slot.generation);
            if generation.is_some() {
                inner.remove(host_id);
            }
            generation
        };
        // Clear the takeover mark only if it names the session actually
        // removed (r16 item 6).
        if let Some(generation) = removed_generation {
            self.clear_live_takeover_generation_locked(host_id, generation)
                .await;
        }
    }

    /// Drop the session only if `generation` is still current (stale WS must not offline a live host).
    pub async fn remove_generation(&self, host_id: &str, generation: u64) -> bool {
        let _authority = self.host_authority(host_id).await.write_owned().await;
        let removed = {
            let mut inner = self.inner.lock().await;
            match inner.get(host_id) {
                Some(slot) if slot.generation == generation => {
                    inner.remove(host_id);
                    true
                }
                _ => false,
            }
        };
        if removed {
            // The guard is still held; clear the mark only when it was this
            // exact generation. A stale older socket's remove_generation(old)
            // matched nothing and never reaches here, so it cannot clear a
            // newer takeover mark (r16 item 6).
            self.clear_live_takeover_generation_locked(host_id, generation)
                .await;
        }
        removed
    }

    /// Carrier for a connected host, if any.
    pub async fn kind_of(&self, host_id: &str) -> Option<TransportKind> {
        self.inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.link.kind())
    }

    /// The authenticated session generation currently registered for a host
    /// (minted per hello, incremented on every reconnect). r12 item 2 binds a
    /// gate dispatch and its land grant to exactly this session.
    pub async fn generation_of(&self, host_id: &str) -> Option<u64> {
        self.inner
            .lock()
            .await
            .get(host_id)
            .map(|slot| slot.generation)
    }

    /// Test support: take the registry lock and hold it until `release`
    /// resolves. This parks a command POST at its `kind_of` lookup — AFTER the
    /// row/merge exist but BEFORE any forward-attempt slot is registered —
    /// which is exactly the pre-registration window the dispatch marker
    /// covers (D-055 round 3, item 1).
    #[doc(hidden)]
    pub async fn test_hold_lock_until(
        &self,
        release: tokio::sync::oneshot::Receiver<()>,
        acquired: tokio::sync::oneshot::Sender<()>,
    ) {
        let _guard = self.inner.lock().await;
        let _ = acquired.send(());
        // Resolves when the sender sends OR drops.
        let _ = release.await;
    }

    /// Host ids with a live Node session.
    pub async fn host_ids(&self) -> Vec<String> {
        self.inner.lock().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::sync::Arc;

    fn dead_link() -> Arc<dyn NodeTransport> {
        Arc::new(ScriptedTransport::new(None))
    }

    /// r13 item 3: session identities are per-boot monotonic and are NEVER
    /// reused after a host fully disconnects. r14 R3 packs a PERSISTED boot
    /// epoch into the high bits, so identities cannot repeat across Hub restarts
    /// either (covered end to end by the restart integration test).
    #[tokio::test]
    async fn session_identities_are_never_reused_after_removal() {
        let nodes = ConnectedNodes::for_tests(1);
        let a = nodes.insert("h".into(), dead_link()).await.generation;
        assert!(nodes.is_current_session("h", a).await);

        // Full disconnect: the host slot is removed.
        assert!(nodes.remove_generation("h", a).await);
        assert!(!nodes.is_current_session("h", a).await);

        // A brand-new socket for the same host must NOT draw `a` again.
        let b = nodes.insert("h".into(), dead_link()).await.generation;
        assert_ne!(a, b, "session identity reused after removal");
        assert!(nodes.is_current_session("h", b).await);
        assert!(
            !nodes.is_current_session("h", a).await,
            "old session revived"
        );

        // Takeover while the first socket is still open: a third session
        // supersedes b; b is no longer current even though it never closed.
        let c = nodes.insert("h".into(), dead_link()).await.generation;
        assert_ne!(b, c);
        assert!(nodes.is_current_session("h", c).await);
        assert!(!nodes.is_current_session("h", b).await);
        assert!(!nodes.is_current_session("h", a).await);

        // Different hosts draw independent values from the same global counter.
        let d = nodes.insert("other".into(), dead_link()).await.generation;
        assert_ne!(c, d);
    }

    /// r16 item 6: the live-takeover mark is keyed by the TAKEOVER generation
    /// and survives a STALE older socket tearing down AFTER the new hello.
    /// Regression for the old unconditional `clear_live_takeover(host)` in the
    /// teardown path: A displaced B while still live; A then ended; the old code
    /// wiped the host's mark, after which B was treated as a genuine reconnect
    /// and inherited A's in-flight Landing grants.
    #[tokio::test]
    async fn a_stale_socket_teardown_does_not_clear_a_newer_takeover_mark() {
        let nodes = ConnectedNodes::for_tests(1);
        // A connects.
        let a = nodes.insert("h".into(), dead_link()).await.generation;
        assert!(nodes.is_current_session("h", a).await);
        // B takes over while A is still live: the takeover mark names B.
        let inserted_b = nodes.insert("h".into(), dead_link()).await;
        let b = inserted_b.generation;
        assert!(inserted_b.displaced_live, "B displaced a live A");
        assert_ne!(a, b);
        assert!(
            nodes.host_had_live_takeover("h", b).await,
            "B is the recorded live takeover"
        );

        // The OLD socket A tears down AFTER B's hello. Its guarded removal is
        // stale and must NOT clear B's takeover mark.
        let stale = nodes.remove_generation("h", a).await;
        assert!(!stale, "A is no longer current; its removal is stale");
        assert!(
            nodes.host_had_live_takeover("h", b).await,
            "A's stale teardown must not clear B's takeover mark"
        );
        assert!(
            nodes.is_current_session("h", b).await,
            "B is still the live session after A's teardown"
        );

        // A FULL disconnect of B (the marked generation) clears the mark again.
        assert!(nodes.remove_generation("h", b).await);
        assert!(!nodes.host_had_live_takeover("h", b).await);
        assert!(!nodes.is_current_session("h", b).await);
    }

    /// r16 item 6: a genuine reconnect into an EMPTY slot is not a takeover and
    /// therefore rebinds (displaced_live=false), and its teardown leaves no
    /// stale mark.
    #[tokio::test]
    async fn a_genuine_reconnect_after_full_disconnect_is_not_a_takeover() {
        let nodes = ConnectedNodes::for_tests(1);
        let a = nodes.insert("h".into(), dead_link()).await.generation;
        assert!(nodes.remove_generation("h", a).await);
        let inserted_b = nodes.insert("h".into(), dead_link()).await;
        assert!(
            !inserted_b.displaced_live,
            "reconnect into an empty slot is not a takeover"
        );
        assert!(
            !nodes
                .host_had_live_takeover("h", inserted_b.generation)
                .await
        );
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
    /// control calls through the shared reply table and leaves bulk reads and
    /// any frame carrying `"park": true` parked, modelling a Node slow to
    /// answer.
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

    /// A transport whose socket half already exited: every frame fails to
    /// queue before it is sent, so calls must report Ok(None) ("not
    /// writable"), not an Err that settles a command as maybe-executed.
    fn dead_transport() -> (WssTransport, PendingRpcs) {
        let pending = new_pending_rpcs();
        let (outbound, inbound) = mpsc::channel::<Value>(4);
        // Drop the receiver before any call: sends now fail without a frame
        // ever being delivered.
        drop(inbound);
        (WssTransport::new(outbound, pending.clone()), pending)
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

    /// Spawn a caller that stays parked (long timeout, never elapses).
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
    /// retryable NODE_BUSY immediately and no call surfaces INTERNAL.
    ///
    /// Deterministic: admitted reads park indefinitely in the pump (long
    /// timeout), so there is no timeout to race; their futures are aborted at
    /// the end, which also re-covers the cancellation cleanup path.
    #[tokio::test]
    async fn bulk_reads_cannot_starve_control() {
        let (transport, pending, answered) = harness();

        let screens: Vec<JoinHandle<CallResult>> = (0..40)
            .map(|_| {
                let transport = transport.clone();
                tokio::spawn(async move {
                    transport
                        .call(METHOD_TTY_SCREEN, json!({}), Duration::from_secs(3_600))
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

        // 24 refusals are already NODE_BUSY; aborting the 16 parked reads
        // before their long timeout still frees the slots.
        let mut refused = 0usize;
        for handle in screens {
            handle.abort();
            match handle.await {
                // Cancellation: the admitted, parked calls.
                Err(join) if join.is_cancelled() => {}
                // Refused at admission: must be the retryable error.
                Ok(Err(HubError::NodeBusy { .. })) => refused += 1,
                other => panic!("unexpected screen outcome: {other:?}"),
            }
        }
        assert_eq!(refused, 40 - MAX_IN_FLIGHT_BULK_READS);
        assert_eq!(pending_len(&pending), 0);
    }

    /// A parked bulk read whose own timeout elapses maps to NODE_BUSY, not a
    /// 500: a slow row is retryable noise.
    #[tokio::test]
    async fn bulk_timeout_is_node_busy() {
        let (transport, pending, _answered) = harness();
        let result = transport
            .call(METHOD_TTY_SCREEN, json!({}), Duration::from_millis(50))
            .await;
        assert!(
            matches!(result, Err(HubError::NodeBusy { .. })),
            "{result:?}"
        );
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

    /// A failed send *before* the frame is queued is Ok(None) ("not
    /// connected"): callers must not mark such a command reconciling.
    #[tokio::test]
    async fn pre_send_failure_is_not_writable() {
        let (transport, pending) = dead_transport();
        let result = transport
            .call(METHOD_INSTANCE_CREATE, json!({}), Duration::from_secs(5))
            .await;
        assert!(matches!(result, Ok(None)), "{result:?}");
        assert_eq!(pending_len(&pending), 0);
    }

    /// A waiter dropped by fail_all_pending *after* a successful send is an
    /// Err: the frame reached the Node and it may have executed, so command
    /// forwarding must reconcile rather than read "not connected".
    #[tokio::test]
    async fn post_send_link_drop_is_lost_reply_error() {
        let (transport, pending, _answered) = harness();
        let parked = spawn_parked(transport, "tty.attach", json!({ "park": true }));
        wait_pending(&pending, 1).await;
        fail_all_pending(&pending);
        let result = tokio::time::timeout(Duration::from_secs(5), parked)
            .await
            .expect("caller finishes on teardown")
            .expect("task joins");
        match result {
            Err(HubError::Internal(message)) if message == "node rpc dropped" => {}
            other => panic!("expected dropped-reply Err, got {other:?}"),
        }
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
