//! Outbound Hub WebSocket JSON-RPC client (`GET /v1/node`).

use crate::DevNode;
use crate::NodeError;
use crate::transport::{Backoff, NodeTransport, TransportMetrics, TransportMetricsSnapshot};
use futures::{SinkExt, StreamExt};
use remuda_protocol::hubnode::{
    self, JournalAppendParams, JournalSeqWatermark, METHOD_JOURNAL_APPEND, METHOD_NODE_HEARTBEAT,
    METHOD_NODE_HELLO, NodeHeartbeatParams, NodeHelloParams, WS_AUTHORIZATION_SCHEME,
};
use remuda_protocol::{Id, PROTOCOL_VERSION, U64};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

mod runtime_wss;
mod uplink;

#[cfg(unix)]
type RuntimeController = crate::daemon::DaemonWssFence;
#[cfg(not(unix))]
type RuntimeController = ();

struct ConnectingTask(Option<tokio::task::JoinHandle<()>>);

impl Drop for ConnectingTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
use crate::transport::hubnode::SeqWatermark;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const DEFAULT_JOURNAL_QUEUE: usize = 32;
const DEFAULT_HUB_QUEUE: usize = 32;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Dial settings for [`WssLink`].
#[derive(Debug, Clone)]
pub struct WssConfig {
    /// `ws://host/v1/node` or `wss://host/v1/node`.
    pub url: String,
    /// Bootstrap token on first enroll, or the host token from a prior hello.
    pub token: String,
    /// Host identity sent in `node.hello`.
    pub host_id: String,
    /// Operator label stored on the Hub host row.
    pub label: String,
    /// Node binary version advertised to Hub.
    pub node_version: String,
    /// CLI inventory included in hello/heartbeat (`cli` array).
    pub cli: Value,
    /// Full nested host inventory included in hello/heartbeat.
    pub host: Option<Value>,
    /// Heartbeat period. Hub lease TTL is 60s; 15s matches Hub limits.
    pub heartbeat_interval: Duration,
    /// Reconnect delay policy. Commands are never replayed.
    pub backoff: Backoff,
    /// Bounded `journal.append` queue (backpressure).
    pub journal_queue: usize,
}

impl WssConfig {
    /// Connect to `ws://{addr}/v1/node` with a bearer token.
    pub fn loopback(addr: std::net::SocketAddr, token: impl Into<String>, host_id: String) -> Self {
        Self {
            url: format!("ws://{addr}/v1/node"),
            token: token.into(),
            host_id,
            label: "remuda-node".into(),
            node_version: env!("CARGO_PKG_VERSION").into(),
            cli: json!([{
                "kind": "claude",
                "version": "2.1.268",
                "path": "/usr/bin/claude",
                "auth": "unknown"
            }]),
            host: None,
            heartbeat_interval: Duration::from_secs(15),
            backoff: Backoff::default(),
            journal_queue: DEFAULT_JOURNAL_QUEUE,
        }
    }

    /// Replace `cli` / `host` with a live PATH probe (installed CLIs only).
    #[must_use]
    pub fn with_collected_inventory(self) -> Self {
        self.with_collected_inventory_from(&crate::inventory::CollectRequest::default())
    }

    /// PATH probe plus operator labels / maxInstances / herdr socket.
    #[must_use]
    pub fn with_collected_inventory_from(
        mut self,
        request: &crate::inventory::CollectRequest,
    ) -> Self {
        let mut snap = crate::inventory::collect(request);
        snap.cli.retain(|entry| entry.path.is_some());
        self.cli = snap.cli_hub_json();
        let mut host = snap.to_hub_host();
        if let Some(obj) = host.as_object_mut() {
            obj.insert("hostId".into(), json!(self.host_id.clone()));
        }
        self.host = Some(host);
        self
    }
}

/// Hub→Node JSON-RPC request. Not stored across reconnects.
#[derive(Debug, Clone)]
pub struct HubRequest {
    /// JSON-RPC id to echo on the reply.
    pub id: Value,
    /// Method (`instance.create`, `instance.send`, …).
    pub method: String,
    /// Params object.
    pub params: Value,
    reply: mpsc::Sender<HubReply>,
}

impl HubRequest {
    /// Complete this request after the application has durably accepted or rejected it.
    pub async fn respond(self, result: Result<Value, NodeError>) -> Result<(), NodeError> {
        if self.id.is_null() {
            return Ok(());
        }
        self.reply
            .send(HubReply {
                id: self.id,
                result,
            })
            .await
            .map_err(|_| NodeError::Disconnected)
    }
}

#[derive(Debug)]
pub(crate) struct HubReply {
    pub id: Value,
    pub result: Result<Value, NodeError>,
}

struct JournalJob {
    instance_id: String,
    event: Value,
    seq: Option<i64>,
    reply: oneshot::Sender<Result<Value, NodeError>>,
}

/// Bounded sender for `journal.append`. `append` waits when the queue is full.
#[derive(Clone)]
pub struct JournalSender {
    tx: mpsc::Sender<JournalJob>,
    metrics: TransportMetrics,
}

impl JournalSender {
    /// Enqueue an observation. Waits when the session is applying backpressure.
    pub async fn append(&self, instance_id: String, event: Value) -> Result<Value, NodeError> {
        self.push(instance_id, event, None).await
    }

    /// Enqueue an observation at a known local sequence (Hub watermark).
    pub async fn append_seq(
        &self,
        instance_id: String,
        seq: i64,
        event: Value,
    ) -> Result<Value, NodeError> {
        self.push(instance_id, event, Some(seq)).await
    }

    async fn push(
        &self,
        instance_id: String,
        event: Value,
        seq: Option<i64>,
    ) -> Result<Value, NodeError> {
        let (reply, rx) = oneshot::channel();
        self.metrics.enqueue(self.tx.capacity() == 0);
        self.tx
            .send(JournalJob {
                instance_id,
                event,
                seq,
                reply,
            })
            .await
            .map_err(|_| NodeError::JournalQueueClosed)?;
        rx.await.map_err(|_| NodeError::JournalQueueClosed)?
    }
}

#[cfg(test)]
fn journal_channel(
    capacity: usize,
) -> (JournalSender, mpsc::Receiver<JournalJob>, TransportMetrics) {
    let metrics = TransportMetrics::new();
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        JournalSender {
            tx,
            metrics: metrics.clone(),
        },
        rx,
        metrics,
    )
}

enum Control {
    Reconnect(oneshot::Sender<Result<(), NodeError>>),
    Shutdown,
}

/// Give the runtime an object source for this connection (D-027): HTTP first,
/// pulling over the carrier when the Hub HTTP origin is unreachable. The
/// unreachable decision is taken once per connection — [`attach_object_source`]
/// runs after every (re)connected hello with a fresh fallback flag. A Node that
/// cannot derive an HTTP base uses the carrier alone.
fn attach_object_source(
    runtime: Option<&runtime_wss::RuntimeLink>,
    ws_url: &str,
    token: &str,
    broker: &Arc<crate::carrier_objects::CarrierObjectBroker>,
) {
    let Some(link) = runtime else {
        return;
    };
    match crate::attachments::HubObjectSource::from_ws_url(ws_url, token) {
        Ok(http) => {
            let source =
                crate::carrier_objects::FallbackObjectSource::new(http, broker.source());
            link.node.set_object_source(Arc::new(source));
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "Node cannot derive a Hub object URL; pulling attachments over the carrier"
            );
            link.node.set_object_source(Arc::new(broker.source()));
        }
    }
}

enum TtyWire {
    Json(Value),
    Binary(Vec<u8>),
}

struct PendingAppend {
    reply: oneshot::Sender<Result<Value, NodeError>>,
    instance_id: String,
    seq: Option<i64>,
}

/// Live outbound WSS session: hello, heartbeat, journal pump, reconnect.
pub struct WssLink {
    /// Host id used in hello.
    pub host_id: String,
    /// Host token returned by the first enroll, if any.
    pub node_token: Option<String>,
    /// Raw `node.hello` result.
    pub hello: Value,
    journal: JournalSender,
    metrics: TransportMetrics,
    control: mpsc::Sender<Control>,
    hub_rx: mpsc::Receiver<HubRequest>,
    task: JoinHandle<()>,
}

impl WssLink {
    /// Dial Hub, complete `node.hello`, and spawn the session loop.
    pub async fn connect(config: WssConfig) -> Result<Self, NodeError> {
        Self::connect_inner(config, None, None).await
    }

    /// Dial Hub and dispatch `instance.*` into a local [`DevNode`], streaming its journal.
    pub async fn connect_runtime(config: WssConfig, node: DevNode) -> Result<Self, NodeError> {
        Self::connect_inner(config, Some(node), None).await
    }

    /// Connect a daemon runtime under a revocable controller lease.
    /// The caller retains the lease and shuts this link down when it is revoked.
    #[cfg(unix)]
    pub async fn connect_runtime_controlled(
        config: WssConfig,
        node: DevNode,
        lease: crate::DaemonWssLease,
    ) -> Result<Self, NodeError> {
        Self::connect_inner(config, Some(node), Some(lease.fence())).await
    }

    /// Persist the decoded enrollment result before exposing or cancelling hello.
    #[cfg(unix)]
    pub async fn connect_runtime_controlled_persisting(
        config: WssConfig,
        node: DevNode,
        lease: crate::DaemonWssLease,
        data_dir: std::path::PathBuf,
    ) -> Result<Self, NodeError> {
        let mut fence = lease.fence();
        fence.enrollment_dir = Some(data_dir);
        Self::connect_inner(config, Some(node), Some(fence)).await
    }

    async fn connect_inner(
        config: WssConfig,
        runtime: Option<DevNode>,
        controller: Option<RuntimeController>,
    ) -> Result<Self, NodeError> {
        let token = config.token.clone();
        let host_id = config.host_id.clone();
        let (journal_tx, journal_rx) = mpsc::channel(config.journal_queue.max(1));
        let (hub_tx, hub_rx) = mpsc::channel(DEFAULT_HUB_QUEUE);
        let (hub_reply_tx, hub_reply_rx) = mpsc::channel(DEFAULT_HUB_QUEUE);
        let (control_tx, control_rx) = mpsc::channel(4);
        let (ready_tx, ready_rx) = oneshot::channel();
        let metrics = TransportMetrics::new();
        let journal = JournalSender {
            tx: journal_tx,
            metrics: metrics.clone(),
        };
        let fence = controller.clone();
        let session = session_task(
            config,
            token,
            runtime,
            controller,
            journal.clone(),
            journal_rx,
            hub_tx,
            hub_reply_tx,
            hub_reply_rx,
            control_rx,
            Some(ready_tx),
            metrics.clone(),
        );
        let task = tokio::spawn(async move {
            #[cfg(unix)]
            if let Some(fence) = fence {
                tokio::select! {
                    biased;
                    _ = fence.revoked() => {},
                    _ = session => {},
                }
                return;
            }
            #[cfg(not(unix))]
            let _ = fence;
            session.await;
        });
        let mut connecting = ConnectingTask(Some(task));
        let hello = ready_rx.await.map_err(|_| NodeError::Disconnected)??;
        let task = connecting.0.take().ok_or(NodeError::Disconnected)?;
        let node_token = hello
            .get("nodeToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            host_id,
            node_token,
            hello,
            journal,
            metrics,
            control: control_tx,
            hub_rx,
            task,
        })
    }

    /// Queue and reconnect counters for this session.
    #[must_use]
    pub fn metrics(&self) -> TransportMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Clone the bounded `journal.append` sender used by this session.
    #[must_use]
    pub fn journal_sender(&self) -> JournalSender {
        self.journal.clone()
    }

    /// `journal.append` with queue backpressure.
    pub async fn append_journal(
        &self,
        instance_id: impl Into<String>,
        event: Value,
    ) -> Result<Value, NodeError> {
        self.journal.append(instance_id.into(), event).await
    }

    /// Next Hub→Node request. The caller must invoke [`HubRequest::respond`].
    pub async fn next_hub_request(&mut self) -> Option<HubRequest> {
        self.hub_rx.recv().await
    }

    /// Close the socket and dial again with the current host token. No Command replay.
    pub async fn reconnect(&self) -> Result<(), NodeError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::Reconnect(tx))
            .await
            .map_err(|_| NodeError::Disconnected)?;
        rx.await.map_err(|_| NodeError::Disconnected)?
    }

    /// Stop the session loop.
    pub async fn shutdown(self) {
        let Self {
            control, mut task, ..
        } = self;
        let _ = control.send(Control::Shutdown).await;
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

/// Low-level WebSocket JSON carrier implementing [`NodeTransport`].
pub struct WssCarrier {
    inner: WsStream,
}

impl WssCarrier {
    /// Dial `/v1/node` with `Authorization: Bearer`.
    pub async fn connect(url: &str, token: &str) -> Result<Self, NodeError> {
        let inner = dial(url, token).await?;
        Ok(Self { inner })
    }
}

impl NodeTransport for WssCarrier {
    fn kind(&self) -> &'static str {
        "outbound-wss"
    }

    async fn send_json(&mut self, value: &Value) -> Result<(), NodeError> {
        send_ws(&mut self.inner, value).await
    }

    async fn recv_json(&mut self) -> Result<Option<Value>, NodeError> {
        recv_ws(&mut self.inner).await
    }

    async fn close(&mut self) -> Result<(), NodeError> {
        self.inner
            .close(None)
            .await
            .map_err(|err| NodeError::Transport(err.to_string()))
    }
}

async fn dial(url: &str, token: &str) -> Result<WsStream, NodeError> {
    let mut request = url
        .into_client_request()
        .map_err(|err| NodeError::Transport(err.to_string()))?;
    let value = format!("{WS_AUTHORIZATION_SCHEME} {token}")
        .parse()
        .map_err(|err| NodeError::Transport(format!("authorization header: {err}")))?;
    request.headers_mut().insert(AUTHORIZATION, value);
    let (stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|err| NodeError::Transport(err.to_string()))?;
    Ok(stream)
}

async fn send_ws(stream: &mut WsStream, value: &Value) -> Result<(), NodeError> {
    let text = serde_json::to_string(value)?;
    stream
        .send(Message::Text(text.into()))
        .await
        .map_err(|err| NodeError::Transport(err.to_string()))
}

async fn recv_ws(stream: &mut WsStream) -> Result<Option<Value>, NodeError> {
    loop {
        match stream.next().await {
            None => return Ok(None),
            Some(Err(err)) => return Err(NodeError::Transport(err.to_string())),
            Some(Ok(Message::Text(text))) => {
                return Ok(Some(serde_json::from_str(&text)?));
            }
            Some(Ok(Message::Binary(bytes))) => {
                return Ok(Some(serde_json::from_slice(&bytes)?));
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = stream.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) => return Ok(None),
        }
    }
}

/// Replace the `resources` block in the outbound inventory with a sample taken
/// now.
///
/// The full `host` inventory is collected once (CLI versions, binary hashes)
/// and stays cached; only CPU/memory must move on their own cadence. Without
/// this refresh every heartbeat re-advertised the connect-time sample and a
/// single build spike froze the host at `cpuPct:100` until the next reconnect.
fn refresh_host_resources(config: &mut WssConfig) {
    let Some(host) = config.host.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    match serde_json::to_value(crate::inventory::sample_resources()) {
        Ok(resources) if resources.is_object() => {
            host.insert("resources".into(), resources);
        }
        _ => {}
    }
}

fn rpc_request(id: &str, method: &str, params: Value) -> Value {
    hubnode::rpc_request(id, method, params)
}

fn rpc_ok(id: Value, result: Value) -> Value {
    hubnode::rpc_ok(id, result)
}

fn take_rpc_error(frame: &Value) -> Option<NodeError> {
    let error = frame.get("error")?;
    Some(NodeError::HubRpc {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("hub error")
            .to_owned(),
    })
}

async fn perform_hello(
    stream: &mut WsStream,
    config: &WssConfig,
    node_epoch: &Id,
    watermarks: &HashMap<String, SeqWatermark>,
    ids: &AtomicU64,
    runtime: Option<&runtime_wss::RuntimeLink>,
) -> Result<Value, NodeError> {
    let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
    let mut params = encode_hello_params(config, node_epoch, watermarks);
    if let Some(runtime) = runtime {
        if !params["host"].is_object() {
            params["host"] = json!({});
        }
        runtime.node.advertise_workspaces(&mut params["host"])?;
        // Always announce the instance inventory, daemon or not. The Hub needs
        // it to reconcile rows this Node no longer owns after a restart; a
        // hello without one leaves those rows running forever, holding
        // placement slots nothing can release.
        params["instances"] = serde_json::to_value(runtime.node.list_instances()?.items)?;
        if runtime.controller.is_some() {
            params["daemon"] = json!(true);
            params["durable"] = json!(true);
            #[cfg(unix)]
            if let Some(controller) = &runtime.controller {
                params["controllerGeneration"] = json!(controller.generation().to_string());
            }
        }
    }
    send_ws(stream, &rpc_request(&id, METHOD_NODE_HELLO, params)).await?;
    let Some(frame) = recv_ws(stream).await? else {
        return Err(NodeError::Disconnected);
    };
    if let Some(err) = take_rpc_error(&frame) {
        return Err(err);
    }
    let result = frame
        .get("result")
        .cloned()
        .ok_or_else(|| NodeError::Transport("hello missing result".into()))?;
    #[cfg(unix)]
    if let Some(data_dir) = runtime
        .and_then(|runtime| runtime.controller.as_ref())
        .and_then(|controller| controller.enrollment_dir.as_ref())
    {
        if let Some(token) = result
            .get("nodeToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
        {
            crate::enroll::persist_host_token(data_dir, token)?;
        }
        crate::enroll::apply_hello_result(data_dir, &result)?;
    }
    Ok(result)
}

async fn session_task(
    mut config: WssConfig,
    mut token: String,
    runtime: Option<DevNode>,
    controller: Option<RuntimeController>,
    journal: JournalSender,
    mut journal_rx: mpsc::Receiver<JournalJob>,
    hub_tx: mpsc::Sender<HubRequest>,
    hub_reply_tx: mpsc::Sender<HubReply>,
    mut hub_reply_rx: mpsc::Receiver<HubReply>,
    mut control_rx: mpsc::Receiver<Control>,
    ready: Option<oneshot::Sender<Result<Value, NodeError>>>,
    metrics: TransportMetrics,
) {
    let ids = AtomicU64::new(1);
    let mut pending: HashMap<String, PendingAppend> = HashMap::new();
    let mut attempt = 0_u32;
    let watermarks = Arc::new(Mutex::new(HashMap::new()));
    let pumps = Arc::new(Mutex::new(HashSet::new()));
    let node_epoch = Id::new("epoch");
    #[cfg(unix)]
    let node_epoch = controller
        .as_ref()
        .map(|controller| Ok(controller.epoch()))
        .unwrap_or(node_epoch);
    let Ok(node_epoch) = node_epoch else {
        if let Some(ready) = ready {
            let _ = ready.send(Err(NodeError::InvalidConfig(
                "could not allocate node epoch".into(),
            )));
        }
        return;
    };
    let runtime = runtime.map(|node| runtime_wss::RuntimeLink {
        controller,
        node,
        hub_url: config
            .url
            .trim_end_matches("/v1/node")
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1),
        journal,
        watermarks: watermarks.clone(),
        pumps,
    });
    // object.pull rides this socket whenever the Hub HTTP origin is
    // unreachable; pending pulls are failed on every reconnect below.
    let (carrier_tx, mut carrier_rx) = mpsc::channel(16);
    let object_broker = crate::carrier_objects::CarrierObjectBroker::new(carrier_tx);
    let mut stream = match dial(&config.url, &token).await {
        Ok(stream) => stream,
        Err(err) => {
            if let Some(ready) = ready {
                let _ = ready.send(Err(err));
            }
            return;
        }
    };
    refresh_host_resources(&mut config);
    let snapshot = runtime_wss::snapshot_watermarks(&watermarks);
    let hello = match perform_hello(
        &mut stream,
        &config,
        &node_epoch,
        &snapshot,
        &ids,
        runtime.as_ref(),
    )
    .await
    {
        Ok(hello) => {
            observe_clock_skew(&hello, &metrics);
            hello
        }
        Err(err) => {
            metrics.hello_rejected();
            if let Some(ready) = ready {
                let _ = ready.send(Err(err));
            }
            return;
        }
    };
    if let Some(new_token) = hello.get("nodeToken").and_then(Value::as_str) {
        token = new_token.to_owned();
        config.token = token.clone();
    }
    // D-027: now that the durable host token is known — it is minted by this
    // very hello on first enroll — point the runtime at the Hub's object
    // store. The same credential authenticates this socket and
    // `GET /v1/objects/{id}`, so no second secret is introduced.
    attach_object_source(runtime.as_ref(), &config.url, &token, &object_broker);
    let mut connection_id = hello
        .get("connectionId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut lease_id = hello
        .pointer("/lease/leaseId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(ready) = ready {
        let _ = ready.send(Ok(hello.clone()));
    }
    if let Some(runtime) = runtime.as_ref() {
        runtime_wss::apply_resume_watermarks(runtime, &hello);
        runtime_wss::resume_runtime_journals(runtime);
    }

    let mut heartbeat = tokio::time::interval(config.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _ = heartbeat.tick().await;

    let (tty_tx, mut tty_rx) = mpsc::channel::<TtyWire>(64);
    if let Some(runtime) = runtime.as_ref() {
        spawn_wss_tty_pump(runtime.node.clone(), tty_tx);
    }

    loop {
        tokio::select! {
            control = control_rx.recv() => {
                match control {
                    None | Some(Control::Shutdown) => break,
                    Some(Control::Reconnect(done)) => {
                        let result = reconnect(
                            &mut stream, &mut config, &mut token, &node_epoch,
                            &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id,
                            &mut pending, runtime.as_ref(), &metrics, &object_broker,
                        ).await;
                        let _ = done.send(result);
                    }
                }
            }
            _ = heartbeat.tick() => {
                let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
                let snapshot = runtime_wss::snapshot_watermarks(&watermarks);
                // Piggyback a fresh load/MemInfo sample on the existing 15 s
                // lease frame: the Hub refuses placement on stale CPU, so the
                // number has to track the machine between hellos.
                refresh_host_resources(&mut config);
                let mut params = encode_heartbeat_params(&config, connection_id.as_deref(), lease_id.as_deref(), &snapshot);
                if let Some(runtime) = runtime.as_ref() {
                    if !params["host"].is_object() { params["host"] = json!({}); }
                    if let Err(error) = runtime.node.advertise_workspaces(&mut params["host"]) {
                        tracing::error!(%error, "cannot read workspace inventory");
                        break;
                    }
                }
                let frame = rpc_request(&id, METHOD_NODE_HEARTBEAT, params);
                if send_ws(&mut stream, &frame).await.is_err()
                    && reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref(), &metrics, &object_broker).await.is_err()
                {
                    break;
                }
            }
            job = journal_rx.recv() => {
                let Some(job) = job else { break; };
                // Buffered events may have become durable while this session
                // was reconnecting. The Hub hello watermark is authoritative.
                if let Some(seq) = job.seq
                    && let Some(mark) = runtime_wss::snapshot_watermarks(&watermarks).get(&job.instance_id)
                    && seq <= mark.seq {
                    let _ = job.reply.send(Ok(json!({"durableSeq":mark.seq.to_string()})));
                    continue;
                }
                let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
                let frame = rpc_request(
                    &id,
                    METHOD_JOURNAL_APPEND,
                    encode_append_params(&job.instance_id, job.seq, job.event),
                );
                match send_ws(&mut stream, &frame).await {
                    Ok(()) => {
                        pending.insert(id, PendingAppend { reply: job.reply, instance_id: job.instance_id, seq: job.seq });
                    }
                    Err(_) => {
                        let _ = job.reply.send(Err(NodeError::Disconnected));
                        if reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref(), &metrics, &object_broker).await.is_err() {
                            break;
                        }
                    }
                }
            }
            reply = hub_reply_rx.recv() => {
                let Some(reply) = reply else { break; };
                let frame = match reply.result {
                    Ok(result) => rpc_ok(reply.id, result),
                    Err(error) => rpc_node_error(reply.id, &error),
                };
                if send_ws(&mut stream, &frame).await.is_err()
                    && reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref(), &metrics, &object_broker).await.is_err()
                {
                    break;
                }
            }
            pull = carrier_rx.recv() => {
                let Some(frame) = pull else { break; };
                if send_ws(&mut stream, &frame).await.is_err()
                    && reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref(), &metrics, &object_broker).await.is_err()
                {
                    break;
                }
            }
            incoming = recv_ws(&mut stream) => {
                match incoming {
                    Ok(Some(frame)) => {
                        handle_incoming(frame, &mut pending, &hub_tx, &hub_reply_tx, runtime.as_ref(), &watermarks, &mut stream, &metrics, &object_broker).await;
                    }
                    Ok(None) | Err(_) => {
                        if reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref(), &metrics, &object_broker).await.is_err() {
                            break;
                        }
                    }
                }
            }
            tty = tty_rx.recv() => {
                let Some(tty) = tty else { continue; };
                let send_ok = match tty {
                    TtyWire::Json(value) => send_ws(&mut stream, &value).await.is_ok(),
                    TtyWire::Binary(bytes) => stream
                        .send(Message::Binary(bytes.into()))
                        .await
                        .is_ok(),
                };
                if !send_ok
                    && reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref(), &metrics, &object_broker).await.is_err()
                {
                    break;
                }
            }
        }
    }
    fail_pending(&mut pending, NodeError::Disconnected, &metrics);
}

fn spawn_wss_tty_pump(node: crate::DevNode, tx: mpsc::Sender<TtyWire>) {
    tokio::spawn(async move {
        let mut events = node.tty().subscribe();
        loop {
            let event = tokio::select! {
                _ = tx.closed() => break,
                event = events.recv() => event,
            };
            match event {
                Ok(crate::TtyEvent::Open {
                    instance_id,
                    stream_id,
                }) => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "tty.frame",
                        "params": {
                            "instanceId": instance_id,
                            "streamId": stream_id,
                            "channel": 1,
                            "offset": "0",
                        }
                    });
                    if tx.send(TtyWire::Json(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(crate::TtyEvent::Mode {
                    instance_id,
                    stream_id,
                    alt_screen,
                }) => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "tty.mode",
                        "params": { "instanceId": instance_id, "streamId": stream_id, "altScreen": alt_screen }
                    });
                    if tx.send(TtyWire::Json(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(crate::TtyEvent::Bytes {
                    stream_id,
                    offset,
                    payload,
                    ..
                }) => match crate::encode_tty_frame(&stream_id, offset, &payload) {
                    Ok(frame) => {
                        if tx.send(TtyWire::Binary(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

async fn handle_incoming(
    frame: Value,
    pending: &mut HashMap<String, PendingAppend>,
    hub_tx: &mpsc::Sender<HubRequest>,
    hub_reply_tx: &mpsc::Sender<HubReply>,
    runtime: Option<&runtime_wss::RuntimeLink>,
    watermarks: &Arc<Mutex<HashMap<String, SeqWatermark>>>,
    stream: &mut WsStream,
    metrics: &TransportMetrics,
    object_broker: &crate::carrier_objects::CarrierObjectBroker,
) {
    // object.pull replies and object.chunk notifications complete attachment
    // fetches; they are neither Hub requests nor journal acknowledgements.
    if object_broker.handle_frame(&frame) {
        return;
    }
    let method = frame.get("method").and_then(Value::as_str);
    if let Some(method) = method {
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let params = frame.get("params").cloned().unwrap_or(json!({}));
        if let Some(runtime) = runtime {
            runtime_wss::dispatch_in_background(
                runtime,
                method.to_owned(),
                params,
                id,
                hub_reply_tx.clone(),
            );
            return;
        }
        let request = HubRequest {
            id: id.clone(),
            method: method.to_owned(),
            params,
            reply: hub_reply_tx.clone(),
        };
        if hub_tx.try_send(request).is_err() && !id.is_null() {
            let _ = send_ws(
                stream,
                &rpc_error(id, -32001, "Node application queue is full"),
            )
            .await;
        }
        return;
    }
    let Some(id) = frame.get("id").and_then(|id| {
        id.as_str()
            .map(str::to_owned)
            .or_else(|| id.as_i64().map(|n| n.to_string()))
    }) else {
        return;
    };
    let Some(waiter) = pending.remove(&id) else {
        return;
    };
    if let Some(err) = take_rpc_error(&frame) {
        if let Some(seq) = waiter.seq.filter(|seq| already_durable(&err, *seq)) {
            runtime_wss::record_watermark(watermarks, &waiter.instance_id, None, seq);
            metrics.duplicate_ack();
            let _ = waiter.reply.send(Ok(json!({ "seq": seq })));
        } else {
            let _ = waiter.reply.send(Err(err));
        }
    } else if let Some(result) = frame.get("result").cloned() {
        let seq = result
            .get("durableSeq")
            .or_else(|| result.get("seq"))
            .and_then(value_i64)
            .or(waiter.seq);
        if let Some(seq) = seq {
            runtime_wss::record_watermark(watermarks, &waiter.instance_id, None, seq);
        }
        metrics.acked();
        let _ = waiter.reply.send(Ok(result));
    } else {
        let _ = waiter.reply.send(Err(NodeError::Transport(
            "rpc response missing result".into(),
        )));
    }
}

fn rpc_node_error(id: Value, error: &NodeError) -> Value {
    let code = match error {
        NodeError::InvalidRequest(_) | NodeError::NotFound { .. } => -32602,
        NodeError::QueueFull => -32001,
        NodeError::DriverUnavailable => -32002,
        NodeError::InteractionExpired => -32005,
        NodeError::InteractionSuperseded { .. } => -32004,
        _ => -32603,
    };
    rpc_error(id, code, &error.to_string())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    hubnode::rpc_error(id, code, message)
}

fn encode_hello_params(
    config: &WssConfig,
    node_epoch: &Id,
    watermarks: &HashMap<String, SeqWatermark>,
) -> Value {
    // Insert the raw host value after the typed conversion: it carries
    // `driverInventory`, which `NodeHostInventory` does not model, and
    // `hello_capabilities` echoes it verbatim into `capabilities`. This is the
    // outbound-WSS twin of the stdio hello — the demo regression was that only
    // stdio advertised the inventory, so `remuda dev` never sent it.
    let host = config.host.clone();
    let mut params = json!(NodeHelloParams {
        host_id: Some(config.host_id.clone()),
        label: Some(config.label.clone()),
        node_version: Some(config.node_version.clone()),
        node_epoch: Some(node_epoch.to_string()),
        enrollment_token: None,
        transport: Some("outbound-wss".into()),
        version: Some(PROTOCOL_VERSION),
        protocol: Some(json!({
            "major": PROTOCOL_VERSION.major,
            "minor": PROTOCOL_VERSION.minor,
            "framing": "websocket-message",
        })),
        host: host
            .clone()
            .and_then(|value| serde_json::from_value(value).ok()),
        capabilities: host
            .as_ref()
            .and_then(crate::transport::hubnode::hello_capabilities),
        cli: Some(config.cli.clone()),
    });
    if let Some(object) = params.as_object_mut() {
        if let Some(host) = host {
            object.insert("host".into(), host);
        }
        object.insert("resumeCursors".into(), json!(resume_cursors(watermarks)));
        object.insert(
            "instanceWatermarks".into(),
            json!(journal_watermarks(watermarks)),
        );
    }
    params
}

fn encode_heartbeat_params(
    config: &WssConfig,
    connection_id: Option<&str>,
    lease_id: Option<&str>,
    watermarks: &HashMap<String, SeqWatermark>,
) -> Value {
    // The Hub heartbeat handler refreshes capabilities the same way the hello
    // handler does, so carry the same inventory on every refresh: a reconnect
    // that never re-ran hello would otherwise leave the host view describing a
    // stale carrier state.
    json!(NodeHeartbeatParams {
        connection_id: connection_id.map(str::to_owned),
        lease_id: lease_id.map(str::to_owned),
        node_version: Some(config.node_version.clone()),
        transport: Some("outbound-wss".into()),
        host: config
            .host
            .clone()
            .and_then(|host| serde_json::from_value(host).ok()),
        cli: Some(config.cli.clone()),
        capabilities: config
            .host
            .as_ref()
            .and_then(crate::transport::hubnode::hello_capabilities),
        instance_watermarks: journal_watermarks(watermarks),
    })
}

fn encode_append_params(instance_id: &str, seq: Option<i64>, event: Value) -> Value {
    json!(JournalAppendParams {
        instance_id: instance_id.to_owned(),
        event: Some(event.clone()),
        events: vec![event],
        seq: seq.map(|seq| json!(seq.to_string())),
        watermark: seq.map(|seq| JournalSeqWatermark {
            journal_id: None,
            instance_id: Some(instance_id.to_owned()),
            durable_seq: Some(json!(seq.to_string())),
            after_seq: None,
            seq: u64_seq(seq),
        }),
    })
}

fn journal_watermarks(watermarks: &HashMap<String, SeqWatermark>) -> Vec<JournalSeqWatermark> {
    watermarks
        .values()
        .map(|mark| JournalSeqWatermark {
            journal_id: mark.journal_id.clone(),
            instance_id: Some(mark.instance_id.clone()),
            durable_seq: Some(json!(mark.seq.to_string())),
            after_seq: Some(json!(mark.seq.to_string())),
            seq: u64_seq(mark.seq),
        })
        .collect()
}

fn resume_cursors(watermarks: &HashMap<String, SeqWatermark>) -> Vec<Value> {
    watermarks
        .values()
        .map(|mark| {
            json!({
                "journalId": mark.journal_id.as_deref().unwrap_or(mark.instance_id.as_str()),
                "afterSeq": mark.seq.to_string(),
                "instanceId": mark.instance_id,
            })
        })
        .collect()
}

fn u64_seq(seq: i64) -> Option<U64> {
    u64::try_from(seq.max(0)).ok().map(U64)
}

fn fail_pending(
    pending: &mut HashMap<String, PendingAppend>,
    error: NodeError,
    metrics: &TransportMetrics,
) {
    metrics.drop_pending(pending.len() as u64);
    for (_, waiter) in pending.drain() {
        let _ = waiter.reply.send(Err(match &error {
            NodeError::Disconnected => NodeError::Disconnected,
            other => NodeError::Transport(other.to_string()),
        }));
    }
}

async fn reconnect(
    stream: &mut WsStream,
    config: &mut WssConfig,
    token: &mut String,
    node_epoch: &Id,
    watermarks: &Arc<Mutex<HashMap<String, SeqWatermark>>>,
    ids: &AtomicU64,
    attempt: &mut u32,
    connection_id: &mut Option<String>,
    lease_id: &mut Option<String>,
    pending: &mut HashMap<String, PendingAppend>,
    runtime: Option<&runtime_wss::RuntimeLink>,
    metrics: &TransportMetrics,
    object_broker: &Arc<crate::carrier_objects::CarrierObjectBroker>,
) -> Result<(), NodeError> {
    metrics.reconnect();
    fail_pending(pending, NodeError::Disconnected, metrics);
    // Pulls in flight belong to the dead socket; their retry rides this one.
    object_broker.fail_all(NodeError::Disconnected);
    let _ = stream.close(None).await;
    loop {
        let delay = config
            .backoff
            .jittered_delay(*attempt, ids.load(Ordering::Relaxed));
        tracing::warn!(
            attempt = *attempt,
            ?delay,
            "hub wss reconnecting; commands are not replayed"
        );
        sleep(delay).await;
        refresh_host_resources(config);
        let snapshot = runtime_wss::snapshot_watermarks(watermarks);
        match dial(&config.url, token).await {
            Ok(mut next) => {
                match perform_hello(&mut next, config, node_epoch, &snapshot, ids, runtime).await {
                    Ok(hello) => {
                        if let Some(new_token) = hello.get("nodeToken").and_then(Value::as_str) {
                            *token = new_token.to_owned();
                            config.token = token.clone();
                        }
                        attach_object_source(
                            runtime.as_ref().copied(),
                            &config.url,
                            token,
                            object_broker,
                        );
                        *connection_id = hello
                            .get("connectionId")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        *lease_id = hello
                            .pointer("/lease/leaseId")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        *stream = next;
                        *attempt = 0;
                        observe_clock_skew(&hello, metrics);
                        if let Some(runtime) = runtime {
                            runtime_wss::apply_resume_watermarks(runtime, &hello);
                            runtime_wss::resume_runtime_journals(runtime);
                        }
                        return Ok(());
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "hello after reconnect failed");
                        metrics.hello_rejected();
                        *attempt = attempt.saturating_add(1);
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "dial after disconnect failed");
                *attempt = attempt.saturating_add(1);
            }
        }
    }
}

pub(crate) fn value_i64(value: &Value) -> Option<i64> {
    hubnode::value_as_i64(value)
}

fn observe_clock_skew(hello: &Value, metrics: &TransportMetrics) {
    let Some(raw) = hello.get("serverTime").and_then(Value::as_str) else {
        return;
    };
    let Ok(parsed) =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
    else {
        return;
    };
    let now = time::OffsetDateTime::now_utc();
    let delta = (now - parsed).unsigned_abs();
    if delta > Duration::from_secs(60) {
        metrics.clock_skew();
        tracing::warn!(%raw, ?delta, "hub serverTime clock skew");
    }
}

pub(crate) fn already_durable(error: &NodeError, seq: i64) -> bool {
    match error {
        NodeError::HubRpc { message, .. } => {
            if message == &format!("journal duplicate seq {seq}") {
                return true;
            }
            let Some((_, gap)) = message.split_once("journal gap: expected ") else {
                return false;
            };
            let Some((expected, got)) = gap.split_once(", got ") else {
                return false;
            };
            match (expected.trim().parse::<i64>(), got.trim().parse::<i64>()) {
                // Older Hubs used a gap error for an already retained prefix.
                // A future gap proves this append was rejected, never durable.
                (Ok(expected), Ok(got)) => got == seq && seq < expected,
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn journal_queue_applies_backpressure() {
        let (sender, mut rx, metrics) = journal_channel(1);
        let first = tokio::spawn({
            let sender = sender.clone();
            async move { sender.append("ins_a".into(), json!({"n":1})).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = tokio::spawn({
            let sender = sender.clone();
            async move { sender.append("ins_b".into(), json!({"n":2})).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !second.is_finished(),
            "second append must wait on a full queue"
        );
        assert!(
            metrics.snapshot().journal_backpressure_waits >= 1,
            "sender must record a wait while the bounded queue is full"
        );

        let job = rx.recv().await.expect("queued");
        assert_eq!(job.instance_id, "ins_a");
        let _ = job.reply.send(Ok(json!({"seq":"1"})));
        first.await.expect("join").expect("first seq");

        let job = rx.recv().await.expect("second queued");
        assert_eq!(job.instance_id, "ins_b");
        let _ = job.reply.send(Ok(json!({"seq":"2"})));
        second.await.expect("join").expect("second seq");
        assert_eq!(metrics.snapshot().journal_enqueued, 2);
    }

    #[tokio::test]
    async fn shutdown_aborts_an_unresponsive_session_task() {
        let (journal, _journal_rx, metrics) = journal_channel(1);
        let (control, _control_rx) = mpsc::channel(1);
        let (_hub_tx, hub_rx) = mpsc::channel(1);
        let task = tokio::spawn(std::future::pending());
        let link = WssLink {
            host_id: "hst_test".into(),
            node_token: None,
            hello: json!({}),
            journal,
            metrics,
            control,
            hub_rx,
            task,
        };

        tokio::time::timeout(Duration::from_secs(2), link.shutdown())
            .await
            .expect("shutdown must remain bounded");
    }

    #[test]
    fn append_params_use_hubnode_batch_and_watermark() {
        let value = encode_append_params("ins_x", Some(4), json!({"kind": "message"}));
        let parsed: JournalAppendParams = serde_json::from_value(value).expect("params");
        assert_eq!(parsed.events_to_append().len(), 1);
        assert_eq!(parsed.seq_i64(), Some(4));
        assert_eq!(
            parsed
                .watermark
                .as_ref()
                .and_then(|mark: &JournalSeqWatermark| mark.durable_i64()),
            Some(4)
        );
    }

    #[test]
    fn already_durable_matches_exact_got_seq() {
        let ten = NodeError::HubRpc {
            code: -32602,
            message: "journal gap: expected 11, got 10".into(),
        };
        assert!(already_durable(&ten, 10));
        assert!(
            !already_durable(&ten, 1),
            "seq 1 must not match suffix got 10"
        );
        let dup = NodeError::HubRpc {
            code: -32602,
            message: "journal duplicate seq 4".into(),
        };
        assert!(already_durable(&dup, 4));
        assert!(!already_durable(&dup, 40));
        for message in [
            "journal gap: expected 17, got 18",
            "id: journal gap: expected 17, got 18",
            "journal gap: expected 18, got 18",
            "journal gap: expected invalid, got 18",
            "journal gap: expected 19, got 180",
        ] {
            let rejected = NodeError::HubRpc {
                code: -32602,
                message: message.into(),
            };
            assert!(
                !already_durable(&rejected, 18),
                "{message} must not promote seq18"
            );
        }
        assert!(already_durable(
            &NodeError::HubRpc {
                code: -32602,
                message: "id: journal gap: expected 19, got 18".into()
            },
            18
        ));
    }

    #[test]
    fn hello_params_include_resume_cursors() {
        let epoch = Id::new("epoch").expect("epoch");
        let host = remuda_protocol::HostId::new();
        let config = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host.as_id().as_str().to_owned(),
        );
        let mut marks = HashMap::new();
        marks.insert(
            "ins_a".into(),
            SeqWatermark {
                instance_id: "ins_a".into(),
                journal_id: None,
                seq: 3,
            },
        );
        let value = encode_hello_params(&config, &epoch, &marks);
        assert_eq!(value["transport"], json!("outbound-wss"));
        assert_eq!(value["cli"][0]["kind"], json!("claude"));
        assert_eq!(value["resumeCursors"][0]["afterSeq"], json!("3"));
        let parsed: NodeHelloParams = serde_json::from_value(value).expect("hello");
        assert_eq!(parsed.persisted_host_id(), Some(config.host_id.as_str()));
    }

    #[test]
    fn wss_hello_carries_the_driver_inventory_under_capabilities() {
        // D-028 §5.1, outbound-WSS twin of the stdio test: `remuda dev` and
        // every real outbound Node use this path. Hardcoding `capabilities:
        // None` here was the bug that left GET /v1/hosts reporting `{}`.
        let epoch = Id::new("epoch").expect("epoch");
        let host_id = remuda_protocol::HostId::new();
        let mut config = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host_id.as_id().as_str().to_owned(),
        );
        config.host = Some(json!({
            "hostname": "lab",
            "driverInventory": [{"kind": "shell-pty", "launchable": true,
                                 "reasonCode": "carrier-native"}],
        }));
        let value = encode_hello_params(&config, &epoch, &HashMap::new());
        assert_eq!(
            value["capabilities"]["driverInventory"][0]["kind"],
            json!("shell-pty")
        );
        assert_eq!(
            value["capabilities"]["driverInventory"][0]["launchable"],
            json!(true)
        );
        // The raw nested host survives the typed conversion so the Hub stores
        // the same descriptor both places.
        assert_eq!(
            value["host"]["driverInventory"][0]["launchable"],
            json!(true)
        );
    }

    #[test]
    fn wss_hello_without_inventory_sends_no_capabilities_rather_than_an_empty_claim() {
        // Absence must read as "not reported", never as "cannot launch".
        let epoch = Id::new("epoch").expect("epoch");
        let host_id = remuda_protocol::HostId::new();
        let mut config = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host_id.as_id().as_str().to_owned(),
        );
        config.host = Some(json!({ "hostname": "lab" }));
        let value = encode_hello_params(&config, &epoch, &HashMap::new());
        assert!(value["capabilities"].is_null());

        // No host at all (loopback config before inventory collection) still
        // sends no capabilities.
        let bare = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host_id.as_id().as_str().to_owned(),
        );
        let value = encode_hello_params(&bare, &epoch, &HashMap::new());
        assert!(value["capabilities"].is_null());
    }

    #[test]
    fn heartbeat_resource_refresh_replaces_only_the_resources_block() {
        let host_id = remuda_protocol::HostId::new();
        let mut config = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host_id.as_id().as_str().to_owned(),
        );
        config.host = Some(json!({
            "hostname": "lab",
            "labels": { "region": "sg" },
            "resources": { "cpuPct": 100, "memPct": 99, "cpuCount": 14 }
        }));
        refresh_host_resources(&mut config);
        let host = config.host.as_ref().expect("host");
        // The frozen spike reading is replaced by a live sample…
        let resources = host.get("resources").expect("resources");
        assert!(resources.get("cpuPct").is_some());
        assert!(resources.get("cpuCount").is_some());
        // …while the rest of the cached inventory is untouched.
        assert_eq!(host["hostname"], json!("lab"));
        assert_eq!(host["labels"]["region"], json!("sg"));

        // A config without host inventory (loopback tests, bare carriers) is a
        // no-op rather than a synthesized partial host frame.
        let mut bare = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host_id.as_id().as_str().to_owned(),
        );
        refresh_host_resources(&mut bare);
        assert!(bare.host.is_none());
    }

    #[test]
    fn wss_heartbeat_refreshes_the_driver_inventory() {
        // The Hub heartbeat handler writes `capabilities` the same way the
        // hello handler does; sending none would leave a stale carrier state
        // in place after flag changes and reconnects.
        let host_id = remuda_protocol::HostId::new();
        let mut config = WssConfig::loopback(
            "127.0.0.1:1".parse().expect("addr"),
            "tok",
            host_id.as_id().as_str().to_owned(),
        );
        config.host = Some(json!({
            "hostname": "lab",
            "driverInventory": [{"kind": "shell-pty", "launchable": false,
                                 "reasonCode": "carrier-not-enabled"}],
        }));
        let value = encode_heartbeat_params(&config, Some("conn"), Some("lease"), &HashMap::new());
        assert_eq!(
            value["capabilities"]["driverInventory"][0]["launchable"],
            json!(false)
        );
        assert_eq!(
            value["capabilities"]["driverInventory"][0]["reasonCode"],
            json!("carrier-not-enabled")
        );
    }
}
