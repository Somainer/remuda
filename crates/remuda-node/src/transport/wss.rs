//! Outbound Hub WebSocket JSON-RPC client (`GET /v1/node`).

use crate::DevNode;
use crate::NodeError;
use crate::transport::{Backoff, NodeTransport};
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
use crate::transport::hubnode::SeqWatermark;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const DEFAULT_JOURNAL_QUEUE: usize = 32;
const DEFAULT_HUB_QUEUE: usize = 32;

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
            heartbeat_interval: Duration::from_secs(15),
            backoff: Backoff::default(),
            journal_queue: DEFAULT_JOURNAL_QUEUE,
        }
    }

    /// Replace `cli` with a live PATH probe in Hub `{kind,version,path,auth}` shape.
    #[must_use]
    pub fn with_collected_inventory(mut self) -> Self {
        self.cli =
            crate::inventory::collect(&crate::inventory::CollectRequest::default()).cli_hub_json();
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
fn journal_channel(capacity: usize) -> (JournalSender, mpsc::Receiver<JournalJob>) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (JournalSender { tx }, rx)
}

enum Control {
    Reconnect(oneshot::Sender<Result<(), NodeError>>),
    Shutdown,
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
    control: mpsc::Sender<Control>,
    hub_rx: mpsc::Receiver<HubRequest>,
    task: JoinHandle<()>,
}

impl WssLink {
    /// Dial Hub, complete `node.hello`, and spawn the session loop.
    pub async fn connect(config: WssConfig) -> Result<Self, NodeError> {
        Self::connect_inner(config, None).await
    }

    /// Dial Hub and dispatch `instance.*` into a local [`DevNode`], streaming its journal.
    pub async fn connect_runtime(config: WssConfig, node: DevNode) -> Result<Self, NodeError> {
        Self::connect_inner(config, Some(node)).await
    }

    async fn connect_inner(config: WssConfig, runtime: Option<DevNode>) -> Result<Self, NodeError> {
        let token = config.token.clone();
        let host_id = config.host_id.clone();
        let (journal_tx, journal_rx) = mpsc::channel(config.journal_queue.max(1));
        let (hub_tx, hub_rx) = mpsc::channel(DEFAULT_HUB_QUEUE);
        let (hub_reply_tx, hub_reply_rx) = mpsc::channel(DEFAULT_HUB_QUEUE);
        let (control_tx, control_rx) = mpsc::channel(4);
        let (ready_tx, ready_rx) = oneshot::channel();
        let journal = JournalSender { tx: journal_tx };
        let task = tokio::spawn(session_task(
            config,
            token,
            runtime,
            journal.clone(),
            journal_rx,
            hub_tx,
            hub_reply_tx,
            hub_reply_rx,
            control_rx,
            Some(ready_tx),
        ));
        let hello = ready_rx.await.map_err(|_| NodeError::Disconnected)??;
        let node_token = hello
            .get("nodeToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            host_id,
            node_token,
            hello,
            journal,
            control: control_tx,
            hub_rx,
            task,
        })
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
        let _ = self.control.send(Control::Shutdown).await;
        let _ = self.task.await;
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
) -> Result<Value, NodeError> {
    let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
    send_ws(
        stream,
        &rpc_request(
            &id,
            METHOD_NODE_HELLO,
            encode_hello_params(config, node_epoch, watermarks),
        ),
    )
    .await?;
    let Some(frame) = recv_ws(stream).await? else {
        return Err(NodeError::Disconnected);
    };
    if let Some(err) = take_rpc_error(&frame) {
        return Err(err);
    }
    frame
        .get("result")
        .cloned()
        .ok_or_else(|| NodeError::Transport("hello missing result".into()))
}

async fn session_task(
    mut config: WssConfig,
    mut token: String,
    runtime: Option<DevNode>,
    journal: JournalSender,
    mut journal_rx: mpsc::Receiver<JournalJob>,
    hub_tx: mpsc::Sender<HubRequest>,
    hub_reply_tx: mpsc::Sender<HubReply>,
    mut hub_reply_rx: mpsc::Receiver<HubReply>,
    mut control_rx: mpsc::Receiver<Control>,
    ready: Option<oneshot::Sender<Result<Value, NodeError>>>,
) {
    let ids = AtomicU64::new(1);
    let mut pending: HashMap<String, PendingAppend> = HashMap::new();
    let mut attempt = 0_u32;
    let watermarks = Arc::new(Mutex::new(HashMap::new()));
    let pumps = Arc::new(Mutex::new(HashSet::new()));
    let Ok(node_epoch) = Id::new("epoch") else {
        if let Some(ready) = ready {
            let _ = ready.send(Err(NodeError::InvalidConfig(
                "could not allocate node epoch".into(),
            )));
        }
        return;
    };
    let runtime = runtime.map(|node| runtime_wss::RuntimeLink {
        node,
        journal,
        watermarks: watermarks.clone(),
        pumps,
    });
    let mut stream = match dial(&config.url, &token).await {
        Ok(stream) => stream,
        Err(err) => {
            if let Some(ready) = ready {
                let _ = ready.send(Err(err));
            }
            return;
        }
    };
    let snapshot = runtime_wss::snapshot_watermarks(&watermarks);
    let hello = match perform_hello(&mut stream, &config, &node_epoch, &snapshot, &ids).await {
        Ok(hello) => hello,
        Err(err) => {
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

    let mut heartbeat = tokio::time::interval(config.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _ = heartbeat.tick().await;

    loop {
        tokio::select! {
            control = control_rx.recv() => {
                match control {
                    None | Some(Control::Shutdown) => break,
                    Some(Control::Reconnect(done)) => {
                        let result = reconnect(
                            &mut stream, &mut config, &mut token, &node_epoch,
                            &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id,
                            &mut pending, runtime.as_ref(),
                        ).await;
                        let _ = done.send(result);
                    }
                }
            }
            _ = heartbeat.tick() => {
                let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
                let snapshot = runtime_wss::snapshot_watermarks(&watermarks);
                let frame = rpc_request(
                    &id,
                    METHOD_NODE_HEARTBEAT,
                    encode_heartbeat_params(&config, connection_id.as_deref(), lease_id.as_deref(), &snapshot),
                );
                if send_ws(&mut stream, &frame).await.is_err()
                    && reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref()).await.is_err()
                {
                    break;
                }
            }
            job = journal_rx.recv() => {
                let Some(job) = job else { break; };
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
                        if reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref()).await.is_err() {
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
                    && reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref()).await.is_err()
                {
                    break;
                }
            }
            incoming = recv_ws(&mut stream) => {
                match incoming {
                    Ok(Some(frame)) => {
                        handle_incoming(frame, &mut pending, &hub_tx, &hub_reply_tx, runtime.as_ref(), &watermarks, &mut stream).await;
                    }
                    Ok(None) | Err(_) => {
                        if reconnect(&mut stream, &mut config, &mut token, &node_epoch, &watermarks, &ids, &mut attempt, &mut connection_id, &mut lease_id, &mut pending, runtime.as_ref()).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    fail_pending(&mut pending, NodeError::Disconnected);
}

async fn handle_incoming(
    frame: Value,
    pending: &mut HashMap<String, PendingAppend>,
    hub_tx: &mpsc::Sender<HubRequest>,
    hub_reply_tx: &mpsc::Sender<HubReply>,
    runtime: Option<&runtime_wss::RuntimeLink>,
    watermarks: &Arc<Mutex<HashMap<String, SeqWatermark>>>,
    stream: &mut WsStream,
) {
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
        host: None,
        capabilities: None,
        cli: Some(config.cli.clone()),
    });
    if let Some(object) = params.as_object_mut() {
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
    json!(NodeHeartbeatParams {
        connection_id: connection_id.map(str::to_owned),
        lease_id: lease_id.map(str::to_owned),
        node_version: Some(config.node_version.clone()),
        transport: Some("outbound-wss".into()),
        host: None,
        cli: Some(config.cli.clone()),
        capabilities: None,
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

fn fail_pending(pending: &mut HashMap<String, PendingAppend>, error: NodeError) {
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
) -> Result<(), NodeError> {
    fail_pending(pending, NodeError::Disconnected);
    let _ = stream.close(None).await;
    loop {
        let delay = config.backoff.delay(*attempt);
        tracing::warn!(
            attempt = *attempt,
            ?delay,
            "hub wss reconnecting; commands are not replayed"
        );
        sleep(delay).await;
        let snapshot = runtime_wss::snapshot_watermarks(watermarks);
        match dial(&config.url, token).await {
            Ok(mut next) => {
                match perform_hello(&mut next, config, node_epoch, &snapshot, ids).await {
                    Ok(hello) => {
                        if let Some(new_token) = hello.get("nodeToken").and_then(Value::as_str) {
                            *token = new_token.to_owned();
                            config.token = token.clone();
                        }
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
                        if let Some(runtime) = runtime {
                            runtime_wss::resume_runtime_journals(runtime);
                        }
                        return Ok(());
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "hello after reconnect failed");
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

pub(crate) fn already_durable(error: &NodeError, seq: i64) -> bool {
    match error {
        NodeError::HubRpc { message, .. } => {
            message.contains("journal gap") && message.contains(&format!("got {seq}"))
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
        let (sender, mut rx) = journal_channel(1);
        let first = tokio::spawn({
            let sender = sender.clone();
            async move { sender.append("ins_a".into(), json!({"n":1})).await }
        });
        let job = rx.recv().await.expect("queued");
        assert_eq!(job.instance_id, "ins_a");

        let second = tokio::spawn({
            let sender = sender.clone();
            async move { sender.append("ins_b".into(), json!({"n":2})).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !second.is_finished(),
            "second append must wait on a full queue"
        );

        let _ = job.reply.send(Ok(json!({"seq":"1"})));
        first.await.expect("join").expect("first seq");

        let job = rx.recv().await.expect("second queued");
        assert_eq!(job.instance_id, "ins_b");
        let _ = job.reply.send(Ok(json!({"seq":"2"})));
        second.await.expect("join").expect("second seq");
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
}
