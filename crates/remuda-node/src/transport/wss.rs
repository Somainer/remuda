//! Outbound Hub WebSocket JSON-RPC client (`GET /v1/node`).

use crate::NodeError;
use crate::transport::{Backoff, NodeTransport};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

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
struct HubReply {
    id: Value,
    result: Result<Value, NodeError>,
}

struct JournalJob {
    instance_id: String,
    event: Value,
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
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(JournalJob {
                instance_id,
                event,
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
        let token = config.token.clone();
        let host_id = config.host_id.clone();
        let (journal_tx, journal_rx) = mpsc::channel(config.journal_queue.max(1));
        let (hub_tx, hub_rx) = mpsc::channel(DEFAULT_HUB_QUEUE);
        let (hub_reply_tx, hub_reply_rx) = mpsc::channel(DEFAULT_HUB_QUEUE);
        let (control_tx, control_rx) = mpsc::channel(4);
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(session_task(
            config,
            token,
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
            journal: JournalSender { tx: journal_tx },
            control: control_tx,
            hub_rx,
            task,
        })
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
    let value = format!("Bearer {token}")
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
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn hello_params(config: &WssConfig) -> Value {
    let mut params = json!({
        "hostId": config.host_id,
        "nodeVersion": config.node_version,
        "label": config.label,
        "nodeEpoch": config.host_id,
    });
    params["cli"] = config.cli.clone();
    params
}

fn heartbeat_params(config: &WssConfig) -> Value {
    json!({
        "nodeVersion": config.node_version,
        "cli": config.cli,
    })
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
    ids: &AtomicU64,
) -> Result<Value, NodeError> {
    let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
    send_ws(
        stream,
        &rpc_request(&id, "node.hello", hello_params(config)),
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
    mut journal_rx: mpsc::Receiver<JournalJob>,
    hub_tx: mpsc::Sender<HubRequest>,
    hub_reply_tx: mpsc::Sender<HubReply>,
    mut hub_reply_rx: mpsc::Receiver<HubReply>,
    mut control_rx: mpsc::Receiver<Control>,
    ready: Option<oneshot::Sender<Result<Value, NodeError>>>,
) {
    let ids = AtomicU64::new(1);
    let mut pending: HashMap<String, oneshot::Sender<Result<Value, NodeError>>> = HashMap::new();
    let mut attempt = 0_u32;
    let mut stream = match dial(&config.url, &token).await {
        Ok(stream) => stream,
        Err(err) => {
            if let Some(ready) = ready {
                let _ = ready.send(Err(err));
            }
            return;
        }
    };
    let hello = match perform_hello(&mut stream, &config, &ids).await {
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
                        fail_pending(&mut pending, NodeError::Disconnected);
                        let result = reconnect(
                            &mut stream,
                            &mut config,
                            &mut token,
                            &ids,
                            &mut attempt,
                        )
                        .await;
                        let _ = done.send(result);
                    }
                }
            }
            _ = heartbeat.tick() => {
                let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
                let frame = rpc_request(&id, "node.heartbeat", heartbeat_params(&config));
                if send_ws(&mut stream, &frame).await.is_err()
                    && reconnect(&mut stream, &mut config, &mut token, &ids, &mut attempt)
                        .await
                        .is_err()
                {
                    fail_pending(&mut pending, NodeError::Disconnected);
                    break;
                }
            }
            job = journal_rx.recv() => {
                let Some(job) = job else { break; };
                let id = format!("n-{}", ids.fetch_add(1, Ordering::Relaxed));
                let frame = rpc_request(
                    &id,
                    "journal.append",
                    json!({
                        "instanceId": job.instance_id,
                        "event": job.event,
                    }),
                );
                match send_ws(&mut stream, &frame).await {
                    Ok(()) => {
                        pending.insert(id, job.reply);
                    }
                    Err(_) => {
                        let _ = job.reply.send(Err(NodeError::Disconnected));
                        if reconnect(&mut stream, &mut config, &mut token, &ids, &mut attempt)
                            .await
                            .is_err()
                        {
                            fail_pending(&mut pending, NodeError::Disconnected);
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
                    && reconnect(&mut stream, &mut config, &mut token, &ids, &mut attempt)
                        .await
                        .is_err()
                {
                    fail_pending(&mut pending, NodeError::Disconnected);
                    break;
                }
            }
            incoming = recv_ws(&mut stream) => {
                match incoming {
                    Ok(Some(frame)) => {
                        handle_incoming(
                            frame,
                            &mut pending,
                            &hub_tx,
                            &hub_reply_tx,
                            &mut stream,
                        )
                        .await;
                    }
                    Ok(None) | Err(_) => {
                        fail_pending(&mut pending, NodeError::Disconnected);
                        if reconnect(&mut stream, &mut config, &mut token, &ids, &mut attempt)
                            .await
                            .is_err()
                        {
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
    pending: &mut HashMap<String, oneshot::Sender<Result<Value, NodeError>>>,
    hub_tx: &mpsc::Sender<HubRequest>,
    hub_reply_tx: &mpsc::Sender<HubReply>,
    stream: &mut WsStream,
) {
    let method = frame.get("method").and_then(Value::as_str);
    if let Some(method) = method {
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let params = frame.get("params").cloned().unwrap_or(json!({}));
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
        let _ = waiter.send(Err(err));
    } else if let Some(result) = frame.get("result").cloned() {
        let _ = waiter.send(Ok(result));
    } else {
        let _ = waiter.send(Err(NodeError::Transport(
            "rpc response missing result".into(),
        )));
    }
}

fn rpc_node_error(id: Value, error: &NodeError) -> Value {
    let code = match error {
        NodeError::InvalidRequest(_) | NodeError::NotFound { .. } => -32602,
        NodeError::QueueFull => -32001,
        NodeError::DriverUnavailable => -32002,
        _ => -32603,
    };
    rpc_error(id, code, &error.to_string())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn fail_pending(
    pending: &mut HashMap<String, oneshot::Sender<Result<Value, NodeError>>>,
    error: NodeError,
) {
    for (_, waiter) in pending.drain() {
        let _ = waiter.send(Err(match &error {
            NodeError::Disconnected => NodeError::Disconnected,
            other => NodeError::Transport(other.to_string()),
        }));
    }
}

async fn reconnect(
    stream: &mut WsStream,
    config: &mut WssConfig,
    token: &mut String,
    ids: &AtomicU64,
    attempt: &mut u32,
) -> Result<(), NodeError> {
    let _ = stream.close(None).await;
    loop {
        let delay = config.backoff.delay(*attempt);
        tracing::warn!(
            attempt = *attempt,
            ?delay,
            "hub wss reconnecting; commands are not replayed"
        );
        sleep(delay).await;
        match dial(&config.url, token).await {
            Ok(mut next) => match perform_hello(&mut next, config, ids).await {
                Ok(hello) => {
                    if let Some(new_token) = hello.get("nodeToken").and_then(Value::as_str) {
                        *token = new_token.to_owned();
                        config.token = token.clone();
                    }
                    *stream = next;
                    *attempt = 0;
                    return Ok(());
                }
                Err(err) => {
                    tracing::warn!(error = %err, "hello after reconnect failed");
                    *attempt = attempt.saturating_add(1);
                }
            },
            Err(err) => {
                tracing::warn!(error = %err, "dial after disconnect failed");
                *attempt = attempt.saturating_add(1);
            }
        }
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
}
