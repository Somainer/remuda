//! Loopback development HTTP and WebSocket server.

use crate::{
    CommandAction, CreateInstanceRequest, DevNode, DevServerConfig, InstanceCommandRequest,
    NodeError, TTY_MAX_INPUT_BYTES, decode_tty_input, encode_tty_frame,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{
        HeaderValue, Method, StatusCode,
        header::{
            ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
            ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, COOKIE, ORIGIN, SET_COOKIE,
        },
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::{SinkExt, StreamExt};
use remuda_protocol::{
    CommandId, EventsSubscribeResult, Id, InstanceId, Snapshot, U64, WorkspaceId,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::Arc,
};
use tokio::{sync::mpsc, task::JoinHandle};

const ACCESS_CODE_HEADER: &str = "x-remuda-access-code";
const ACCESS_CODE_COOKIE: &str = "remuda_dev_access_code";
const MAX_JSON_FRAME_BYTES: usize = 1024 * 1024;
const DEFAULT_EVENT_LIMIT: usize = 128;

#[derive(Clone)]
struct AppState {
    node: DevNode,
    access_code: Option<Arc<str>>,
}

#[derive(Clone)]
struct AccessPolicy {
    access_code: Option<Arc<str>>,
    allowed_origins: Arc<BTreeSet<String>>,
}

/// Bound local development server.
pub struct DevServer {
    config: DevServerConfig,
    node: DevNode,
}

impl DevServer {
    /// Compose the in-memory Node and FakeDriver registry.
    pub fn new(config: DevServerConfig) -> Result<Self, NodeError> {
        let node = DevNode::new(&config)?;
        Ok(Self { config, node })
    }

    /// Serve until shutdown or listener failure.
    pub async fn serve(self) -> Result<(), NodeError> {
        let listener = tokio::net::TcpListener::bind(self.config.bind_addr).await?;
        tracing::info!(address = %listener.local_addr()?, "remuda dev listening");
        axum::serve(listener, dev_router(self.node, &self.config)).await?;
        Ok(())
    }

    /// Access the composed Node before serving, primarily for local tests.
    pub fn node(&self) -> &DevNode {
        &self.node
    }
}

/// Build the local router without binding a socket.
pub fn dev_router(node: DevNode, config: &DevServerConfig) -> Router {
    let access_code = config.access_code().map(Arc::<str>::from);
    let state = AppState {
        node,
        access_code: access_code.clone(),
    };
    let policy = AccessPolicy {
        access_code,
        allowed_origins: Arc::new(config.allowed_origins.iter().cloned().collect()),
    };

    Router::new()
        .route("/healthz", get(health))
        .route("/v1/dev/session", post(create_dev_session))
        .route("/v1/instances", get(list_instances).post(create_instance))
        .route("/v1/instances/{id}", get(get_instance))
        .route("/v1/instances/{id}/commands", post(submit_command))
        .route("/v1/instances/{id}/journal", get(read_journal))
        .route("/v1/instances/{id}/follow", get(follow_upgrade))
        .route("/v1/instances/{id}/tty", get(tty_upgrade))
        .route("/v1/commands/{id}", get(get_command))
        .route("/v1/rpc", post(http_rpc))
        .route("/v1/client", get(client_upgrade))
        .layer(DefaultBodyLimit::max(MAX_JSON_FRAME_BYTES))
        .layer(middleware::from_fn_with_state(policy, authorize))
        .with_state(state)
}

async fn authorize(State(policy): State<AccessPolicy>, request: Request, next: Next) -> Response {
    let origin = match request.headers().get(ORIGIN) {
        Some(value) => match value.to_str() {
            Ok(value) if policy.allowed_origins.contains(value) => Some(value.to_owned()),
            _ => {
                return api_response(
                    StatusCode::FORBIDDEN,
                    "origin_not_allowed",
                    "origin is not allowed",
                );
            }
        },
        None => None,
    };

    if request.method() == Method::OPTIONS {
        return add_cors(StatusCode::NO_CONTENT.into_response(), origin.as_deref());
    }

    if let Some(expected) = policy.access_code.as_deref() {
        let header_matches = request
            .headers()
            .get(ACCESS_CODE_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|candidate| constant_time_eq(candidate.as_bytes(), expected.as_bytes()));
        let cookie_matches = request
            .headers()
            .get(COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|cookies| cookie_value(cookies, ACCESS_CODE_COOKIE))
            .is_some_and(|candidate| {
                constant_time_eq(candidate.as_bytes(), cookie_token(expected).as_bytes())
            });
        if !header_matches && !cookie_matches {
            return add_cors(
                api_response(
                    StatusCode::UNAUTHORIZED,
                    "access_code_required",
                    "a valid development access code is required",
                ),
                origin.as_deref(),
            );
        }
    }

    add_cors(next.run(request).await, origin.as_deref())
}

fn add_cors(mut response: Response, origin: Option<&str>) -> Response {
    let Some(origin) = origin.and_then(|value| HeaderValue::from_str(value).ok()) else {
        return response;
    };
    let headers = response.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type,x-remuda-access-code"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,POST,OPTIONS"),
    );
    response
}

fn cookie_value<'a>(cookies: &'a str, name: &str) -> Option<&'a str> {
    cookies.split(';').find_map(|entry| {
        let (candidate, value) = entry.trim().split_once('=')?;
        (candidate == name).then_some(value)
    })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let maximum = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..maximum {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn cookie_token(access_code: &str) -> String {
    format!("{:x}", Sha256::digest(access_code.as_bytes()))
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn create_dev_session(State(state): State<AppState>) -> Response {
    let Some(access_code) = state.access_code.as_deref() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let cookie = format!(
        "{ACCESS_CODE_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
        cookie_token(access_code)
    );
    let mut response = StatusCode::NO_CONTENT.into_response();
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            response.headers_mut().insert(SET_COOKIE, value);
            response
        }
        Err(_) => api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cookie_encoding_failed",
            "development session cookie could not be encoded",
        ),
    }
}

async fn list_instances(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(serde_json::to_value(state.node.list_instances()?)?))
}

async fn create_instance(
    State(state): State<AppState>,
    Json(request): Json<CreateInstanceRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let result = state.node.create_instance(request).await?;
    Ok((StatusCode::CREATED, Json(serde_json::to_value(result)?)))
}

async fn get_instance(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let instance_id = parse_instance_id(&raw_id)?;
    Ok(Json(serde_json::to_value(
        state.node.get_instance(&instance_id)?,
    )?))
}

async fn submit_command(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    Json(request): Json<InstanceCommandRequest>,
) -> ApiResult<Json<Value>> {
    let instance_id = parse_instance_id(&raw_id)?;
    let result = state.node.submit_command(&instance_id, request).await?;
    Ok(Json(serde_json::to_value(result)?))
}

async fn get_command(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let command_id = CommandId::from_str(&raw_id).map_err(NodeError::from)?;
    Ok(Json(serde_json::to_value(
        state.node.get_command(&command_id)?,
    )?))
}

#[derive(Debug, Default, Deserialize)]
struct JournalQuery {
    #[serde(default)]
    from_seq: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn read_journal(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    Query(query): Query<JournalQuery>,
) -> ApiResult<Json<Value>> {
    let instance = state.node.get_instance(&parse_instance_id(&raw_id)?)?;
    let page = state.node.read_journal(
        &instance.journal_id,
        query.from_seq.map(U64),
        query.limit.unwrap_or(DEFAULT_EVENT_LIMIT),
    )?;
    Ok(Json(serde_json::to_value(page)?))
}

#[derive(Debug, Default, Deserialize)]
struct FollowQuery {
    #[serde(default)]
    from_seq: Option<u64>,
}

async fn follow_upgrade(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    Query(query): Query<FollowQuery>,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let instance_id = parse_instance_id(&raw_id)?;
    state.node.get_instance(&instance_id)?;
    Ok(upgrade
        .max_message_size(MAX_JSON_FRAME_BYTES)
        .on_upgrade(move |socket| follow_socket(socket, state.node, instance_id, query.from_seq))
        .into_response())
}

async fn tty_upgrade(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let instance_id = parse_instance_id(&raw_id)?;
    state.node.get_instance(&instance_id)?;
    Ok(upgrade
        .max_message_size(MAX_JSON_FRAME_BYTES)
        .on_upgrade(move |socket| tty_socket(socket, state.node, instance_id))
        .into_response())
}

/// The part of a live TTY frame at `offset` the client has not been shown.
///
/// `None` when the snapshot already carried all of it. A frame that straddles
/// `resume_from` is trimmed to its unseen tail so the client's offset
/// accounting stays monotonic and no byte is painted twice.
fn live_tail(offset: u64, payload: &[u8], resume_from: u64) -> Option<(u64, &[u8])> {
    if offset >= resume_from {
        return Some((offset, payload));
    }
    let skip = usize::try_from(resume_from - offset).ok()?;
    let tail = payload.get(skip..)?;
    (!tail.is_empty()).then_some((resume_from, tail))
}

async fn tty_socket(
    mut socket: WebSocket,
    node: DevNode,
    instance_id: remuda_protocol::InstanceId,
) {
    // Subscribe BEFORE the snapshot, never after. `attach` reads the ring at
    // one instant; bytes the child writes between that read and the
    // subscription belong to neither, so this client never sees them and only
    // a reconnect (whose snapshot is taken later) recovers them. That is the
    // same ordering `LocalStore::subscribe` documents for the journal, and the
    // window widens exactly when the machine is loaded — a short-lived process
    // whose only output lands in it looks like a terminal that printed nothing.
    let mut events = node.tty().subscribe();
    let attached = match node.tty().attach(&instance_id).await {
        Ok(attached) => attached,
        Err(error) => {
            tracing::debug!(%error, "tty attach on local socket");
            return;
        }
    };
    let stream_id = attached.stream_id.clone();
    // Everything up to here is painted by the snapshot below; a live frame
    // that straddles the boundary is forwarded from this offset on.
    let mut resume_from = attached.next_offset;
    if attached.alt_screen.is_some() || attached.progress.is_some() {
        // Alt-screen is included only when observed; progress rides the same
        // notice and is null when the emulator has no OSC 9;4 evidence yet.
        let mut notice = json!({"type": "tty.mode", "instanceId": instance_id,
            "streamId": stream_id, "progress": attached.progress.map(crate::tty::progress_json)});
        if let Some(alt_screen) = attached.alt_screen {
            notice["altScreen"] = json!(alt_screen);
        }
        if socket
            .send(Message::Text(notice.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
    if !attached.snapshot.is_empty() {
        match encode_tty_frame(&stream_id, attached.available_from, &attached.snapshot) {
            Ok(frame) => {
                let _ = socket.send(Message::Binary(frame.into())).await;
            }
            Err(error) => tracing::error!(%error, "failed to encode TTY snapshot"),
        }
    }
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break; };
                match msg {
                    Message::Binary(bytes) => {
                        if let Ok((_, payload)) = decode_tty_input(&bytes, TTY_MAX_INPUT_BYTES as u32)
                            && node.tty().write_bytes(&instance_id, &payload).await.is_err()
                        {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&text)
                            && value.get("type").and_then(Value::as_str) == Some("tty.resize")
                        {
                            let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                            let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                            let _ = node.tty().resize(&instance_id, cols, rows).await;
                        }
                    }
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(crate::TtyEvent::Mode { instance_id: id, stream_id: sid, alt_screen })
                        if id == instance_id =>
                    {
                        let notice = json!({"type": "tty.mode", "instanceId": id,
                            "streamId": sid, "altScreen": alt_screen});
                        if socket.send(Message::Text(notice.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(crate::TtyEvent::Progress { instance_id: id, stream_id: sid, progress })
                        if id == instance_id =>
                    {
                        let notice = json!({"type": "tty.mode", "instanceId": id,
                            "streamId": sid,
                            "progress": crate::tty::progress_json(progress)});
                        if socket.send(Message::Text(notice.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(crate::TtyEvent::Bytes { instance_id: id, stream_id: sid, offset, payload })
                        if id == instance_id =>
                    {
                        // Subscribing first means a frame can overlap the
                        // snapshot; forward only the part the client has not
                        // been painted yet, and never rewind the offset.
                        let Some((offset, payload)) = live_tail(offset, &payload, resume_from)
                        else {
                            continue;
                        };
                        resume_from = offset.saturating_add(payload.len() as u64);
                        if let Ok(frame) = encode_tty_frame(&sid, offset, payload)
                            && socket.send(Message::Binary(frame.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FollowControl {
    Subscribe {
        #[serde(rename = "instanceId")]
        instance_id: InstanceId,
        #[serde(default, rename = "fromSeq")]
        from_seq: Option<U64>,
    },
    Unsubscribe {
        #[serde(rename = "subscriptionId")]
        subscription_id: Id,
    },
}

struct PreparedSubscription {
    result: EventsSubscribeResult,
    receiver: tokio::sync::broadcast::Receiver<remuda_protocol::JournalEvent>,
    watermark: U64,
}

fn prepare_subscription(
    node: &DevNode,
    instance_id: &InstanceId,
    after_seq: Option<U64>,
    connection_id: &Id,
) -> Result<PreparedSubscription, NodeError> {
    let receiver = node.subscribe(instance_id)?;
    let snapshot = node.snapshot(instance_id)?;
    let durable_seq = snapshot.as_of_seq;
    let watermark = U64(after_seq.unwrap_or_default().0.max(durable_seq.0));
    Ok(PreparedSubscription {
        result: EventsSubscribeResult {
            subscription_id: Id::new("sub")?,
            journal_id: snapshot.instance.journal_id.clone(),
            floor_seq: U64(1),
            durable_seq,
            snapshot: Some(Snapshot::Instance(Box::new(snapshot))),
            replay_from_seq: U64(watermark.0.saturating_add(1)),
            next_cursor: None,
            connection_id: connection_id.clone(),
        },
        receiver,
        watermark,
    })
}

fn spawn_subscription(
    mut prepared: PreparedSubscription,
    outgoing: mpsc::Sender<Message>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match prepared.receiver.recv().await {
                Ok(event) => {
                    let (_, sequence, _) = event.position();
                    if sequence <= prepared.watermark {
                        continue;
                    }
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "events.batch",
                        "params": {
                            "subscriptionId": prepared.result.subscription_id,
                            "journalId": prepared.result.journal_id,
                            "fromSeq": sequence,
                            "toSeq": sequence,
                            "events": [event],
                            "durableSeq": sequence,
                        }
                    });
                    if queue_json(&outgoing, notification).await.is_err() {
                        return;
                    }
                    prepared.watermark = sequence;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    let frame = json!({
                        "type": "resync_required",
                        "subscriptionId": prepared.result.subscription_id,
                        "journalId": prepared.result.journal_id,
                        "afterSeq": prepared.watermark,
                        "skipped": skipped,
                    });
                    let _ = queue_json(&outgoing, frame).await;
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

async fn follow_socket(
    socket: WebSocket,
    node: DevNode,
    initial_instance_id: InstanceId,
    from_seq: Option<u64>,
) {
    let connection_id = match Id::new("epoch") {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to allocate follow connection identity");
            return;
        }
    };
    let (mut sink, mut source) = socket.split();
    let (outgoing, mut outgoing_rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut subscriptions = BTreeMap::<String, JoinHandle<()>>::new();

    match prepare_subscription(
        &node,
        &initial_instance_id,
        from_seq.map(U64),
        &connection_id,
    ) {
        Ok(prepared) => {
            let key = prepared.result.subscription_id.to_string();
            let snapshot_frame = json!({"type": "snapshot", "result": prepared.result});
            if queue_json(&outgoing, snapshot_frame).await.is_ok() {
                subscriptions.insert(key, spawn_subscription(prepared, outgoing.clone()));
            }
        }
        Err(error) => {
            let _ = queue_json(&outgoing, socket_error(error.to_string())).await;
        }
    }

    while let Some(message) = source.next().await {
        let Ok(message) = message else {
            break;
        };
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_JSON_FRAME_BYTES {
            let _ = queue_json(
                &outgoing,
                socket_error("control frame exceeds local limit".to_owned()),
            )
            .await;
            break;
        }
        match serde_json::from_str::<FollowControl>(&text) {
            Ok(FollowControl::Subscribe {
                instance_id,
                from_seq,
            }) => match prepare_subscription(&node, &instance_id, from_seq, &connection_id) {
                Ok(prepared) => {
                    let key = prepared.result.subscription_id.to_string();
                    let snapshot_frame = json!({"type": "snapshot", "result": prepared.result});
                    if queue_json(&outgoing, snapshot_frame).await.is_ok() {
                        subscriptions.insert(key, spawn_subscription(prepared, outgoing.clone()));
                    }
                }
                Err(error) => {
                    let _ = queue_json(&outgoing, socket_error(error.to_string())).await;
                }
            },
            Ok(FollowControl::Unsubscribe { subscription_id }) => {
                if let Some(task) = subscriptions.remove(subscription_id.as_str()) {
                    task.abort();
                }
            }
            Err(error) => {
                let _ = queue_json(&outgoing, socket_error(error.to_string())).await;
            }
        }
    }

    for (_, task) in subscriptions {
        task.abort();
    }
    drop(outgoing);
    let _ = writer.await;
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope {
    jsonrpc: String,
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn http_rpc(State(state): State<AppState>, Json(envelope): Json<RpcEnvelope>) -> Json<Value> {
    let response = if envelope.jsonrpc != "2.0" {
        rpc_failure(&envelope.id, -32600, "jsonrpc must be 2.0")
    } else {
        match dispatch_rpc(&state.node, &envelope.method, envelope.params, None).await {
            Ok(result) => rpc_success(&envelope.id, result),
            Err(error) => rpc_node_failure(&envelope.id, error),
        }
    };
    Json(response)
}

async fn client_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(MAX_JSON_FRAME_BYTES)
        .on_upgrade(move |socket| client_socket(socket, state.node))
        .into_response()
}

async fn client_socket(socket: WebSocket, node: DevNode) {
    let connection_id = match Id::new("epoch") {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "failed to allocate client connection identity");
            return;
        }
    };
    let (mut sink, mut source) = socket.split();
    let (outgoing, mut outgoing_rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut greeted = false;
    let mut subscriptions = BTreeMap::<String, JoinHandle<()>>::new();

    while let Some(message) = source.next().await {
        let Ok(message) = message else {
            break;
        };
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_JSON_FRAME_BYTES {
            let _ = queue_json(
                &outgoing,
                rpc_failure("", -32600, "request exceeds local frame limit"),
            )
            .await;
            break;
        }
        let envelope = match serde_json::from_str::<RpcEnvelope>(&text) {
            Ok(value) => value,
            Err(error) => {
                let _ = queue_json(&outgoing, rpc_failure("", -32700, &error.to_string())).await;
                continue;
            }
        };
        if envelope.jsonrpc != "2.0" {
            let _ = queue_json(
                &outgoing,
                rpc_failure(&envelope.id, -32600, "jsonrpc must be 2.0"),
            )
            .await;
            continue;
        }
        if !greeted && envelope.method != "runtime.hello" {
            let _ = queue_json(
                &outgoing,
                rpc_failure(
                    &envelope.id,
                    -32001,
                    "runtime.hello must be the first request",
                ),
            )
            .await;
            continue;
        }

        if envelope.method == "events.subscribe" {
            match rpc_prepare_subscription(&node, &envelope.params, &connection_id) {
                Ok(prepared) => {
                    let key = prepared.result.subscription_id.to_string();
                    let result = match serde_json::to_value(&prepared.result) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = queue_json(
                                &outgoing,
                                rpc_failure(&envelope.id, -32603, &error.to_string()),
                            )
                            .await;
                            continue;
                        }
                    };
                    if queue_json(&outgoing, rpc_success(&envelope.id, result))
                        .await
                        .is_ok()
                    {
                        subscriptions.insert(key, spawn_subscription(prepared, outgoing.clone()));
                    }
                }
                Err(error) => {
                    let _ = queue_json(&outgoing, rpc_node_failure(&envelope.id, error)).await;
                }
            }
            continue;
        }

        if envelope.method == "events.unsubscribe" {
            let result =
                parse_id_field::<Id>(&envelope.params, "subscriptionId").map(|subscription_id| {
                    if let Some(task) = subscriptions.remove(subscription_id.as_str()) {
                        task.abort();
                    }
                    Value::Null
                });
            let response = match result {
                Ok(value) => rpc_success(&envelope.id, value),
                Err(error) => rpc_node_failure(&envelope.id, error),
            };
            let _ = queue_json(&outgoing, response).await;
            continue;
        }

        if envelope.method == "tty.attach" {
            match tty_attach(&node, &envelope.params).await {
                Ok((result, frames)) => {
                    let _ = queue_json(&outgoing, rpc_success(&envelope.id, result)).await;
                    for frame in frames {
                        let _ = outgoing.send(Message::Binary(frame.into())).await;
                    }
                }
                Err(error) => {
                    let _ = queue_json(&outgoing, rpc_node_failure(&envelope.id, error)).await;
                }
            }
            continue;
        }

        let response = match dispatch_rpc(
            &node,
            &envelope.method,
            envelope.params,
            Some(&connection_id),
        )
        .await
        {
            Ok(result) => {
                if envelope.method == "runtime.hello" {
                    greeted = true;
                }
                rpc_success(&envelope.id, result)
            }
            Err(error) => rpc_node_failure(&envelope.id, error),
        };
        if queue_json(&outgoing, response).await.is_err() {
            break;
        }
    }

    for (_, task) in subscriptions {
        task.abort();
    }
    drop(outgoing);
    let _ = writer.await;
}

fn rpc_prepare_subscription(
    node: &DevNode,
    params: &Value,
    connection_id: &Id,
) -> Result<PreparedSubscription, NodeError> {
    let journal_id = parse_id_field::<Id>(params, "journalId")?;
    let instance_id = node.instance_for_journal(&journal_id)?;
    prepare_subscription(
        node,
        &instance_id,
        optional_u64_field(params, "afterSeq")?,
        connection_id,
    )
}

/// Handle a Hub→Node JSON-RPC method against a live [`DevNode`].
pub async fn dispatch_hub_rpc(
    node: &DevNode,
    method: &str,
    params: Value,
) -> Result<Value, NodeError> {
    dispatch_rpc(node, method, params, None).await
}

async fn dispatch_rpc(
    node: &DevNode,
    method: &str,
    params: Value,
    connection_id: Option<&Id>,
) -> Result<Value, NodeError> {
    match method {
        "runtime.hello" => runtime_hello(&params, connection_id),
        "host.list" => Ok(json!({"items": [node.host()], "nextCursor": null})),
        "host.get" => {
            let requested = parse_id_field::<remuda_protocol::HostId>(&params, "hostId")?;
            let host = node.host();
            if requested != host.meta.id {
                return Err(NodeError::NotFound {
                    entity: "host",
                    id: requested.as_id().to_string(),
                });
            }
            serde_json::to_value(host).map_err(NodeError::from)
        }
        "workspace.list" => {
            let mut snapshot = node.workspace_snapshot()?;
            snapshot["items"] = serde_json::to_value(node.workspaces()?)?;
            snapshot["nextCursor"] = Value::Null;
            Ok(snapshot)
        }
        "workspace.register" | "workspace.unregister" => node.workspace_rpc(method, params),
        "workspace.get" => {
            let requested = parse_id_field::<WorkspaceId>(&params, "workspaceId")?;
            let workspace = node
                .workspaces()?
                .into_iter()
                .find(|workspace| requested == workspace.meta.id)
                .ok_or_else(|| NodeError::NotFound {
                    entity: "workspace",
                    id: requested.as_id().to_string(),
                })?;
            serde_json::to_value(workspace).map_err(NodeError::from)
        }
        "worktree.list" => node.worktree_rpc_capped(method, &params).await,
        "worker.provision" => node.provision_worker_capped(&params).await,
        "worker.remove" => node.remove_worker(&params).await,
        method if crate::gate::is_gate_method(method) => {
            node.dispatch_gate_rpc(method, &params).await
        }
        "host.doctor" => node.doctor().await,
        "host.resources" => {
            let resources = serde_json::to_value(crate::inventory::sample_resources())?;
            Ok(json!({ "resources": resources }))
        }
        "worktree.create" => node.worktree_rpc_capped(method, &params).await,
        method if crate::workspace_scm::is_scm_method(method) => {
            crate::workspace_scm::handle_rpc_capped(node, method, &params).await
        }
        method if crate::subagent::is_subagent_method(method) => {
            crate::subagent::handle_rpc(node, method, &params).await
        }
        "instance.list" => {
            let mut page = node.list_instances()?;
            if let Some(raw_workspace) = params.get("workspaceId").and_then(Value::as_str) {
                let workspace_id = WorkspaceId::from_str(raw_workspace)?;
                page.items
                    .retain(|instance| instance.workspace_id == workspace_id);
            }
            if let Some(raw_host) = params.get("hostId").and_then(Value::as_str) {
                let host_id = remuda_protocol::HostId::from_str(raw_host)?;
                page.items.retain(|instance| instance.host_id == host_id);
            }
            if let Some(kind) = params.get("kind").and_then(Value::as_str) {
                page.items.retain(|instance| {
                    serde_json::to_value(instance.kind)
                        .ok()
                        .as_ref()
                        .and_then(Value::as_str)
                        == Some(kind)
                });
            }
            serde_json::to_value(page).map_err(NodeError::from)
        }
        "instance.get" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            serde_json::to_value(node.get_instance(&instance_id)?).map_err(NodeError::from)
        }
        "instance.create" => {
            let mut request: CreateInstanceRequest = serde_json::from_value(
                params
                    .get("spec")
                    .cloned()
                    .unwrap_or_else(|| params.clone()),
            )?;
            if request.instance_id.is_none()
                && let Some(raw_id) = params.get("instanceId").and_then(Value::as_str)
            {
                request.instance_id = Some(InstanceId::from_str(raw_id)?);
            }
            if request.prompt.is_empty()
                && let Some(prompt) = params
                    .get("initialInput")
                    .and_then(|input| input.get("text"))
                    .and_then(Value::as_str)
            {
                request.prompt = prompt.to_owned();
            }
            serde_json::to_value(node.create_instance(request).await?).map_err(NodeError::from)
        }
        // D-026: same create path, with the native session to continue.
        "instance.resume" => {
            let mut request: CreateInstanceRequest = serde_json::from_value(
                params
                    .get("spec")
                    .cloned()
                    .unwrap_or_else(|| params.clone()),
            )?;
            if request.resume_session_id.is_none() {
                request.resume_session_id = params
                    .get("resumeSessionId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
            }
            if request.resume_session_id.is_none() {
                return Err(NodeError::InvalidRequest(
                    "instance.resume requires resumeSessionId".to_owned(),
                ));
            }
            if request.resumed_from.is_none()
                && let Some(raw_id) = params.get("resumedFrom").and_then(Value::as_str)
            {
                request.resumed_from = Some(InstanceId::from_str(raw_id)?);
            }
            if request.instance_id.is_none()
                && let Some(raw_id) = params.get("instanceId").and_then(Value::as_str)
            {
                request.instance_id = Some(InstanceId::from_str(raw_id)?);
            }
            serde_json::to_value(node.create_instance(request).await?).map_err(NodeError::from)
        }
        "instance.configure" => {
            crate::transport::hubnode::dispatch_method(node, method, params).await
        }
        "instance.send" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            let prompt = prompt_from_params(&params).ok_or_else(|| {
                NodeError::InvalidRequest("instance.send requires input.text".to_owned())
            })?;
            submit_rpc_command(
                node,
                &params,
                instance_id,
                CommandAction::Send,
                Some(prompt),
            )
            .await
        }
        "instance.cancel" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            submit_rpc_command(node, &params, instance_id, CommandAction::Cancel, None).await
        }
        "instance.close" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            submit_rpc_command(node, &params, instance_id, CommandAction::Close, None).await
        }
        "instance.purge" => {
            // Hub-driven session deletion. Idempotent: purging an Instance this
            // Node never had reports `purged: false` rather than failing.
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            node.purge_instance(&instance_id).await
        }
        "command.get" => {
            let command_id = parse_id_field::<CommandId>(&params, "commandId")?;
            serde_json::to_value(node.get_command(&command_id)?).map_err(NodeError::from)
        }
        "interaction.list" => Ok(json!({"items": [], "nextCursor": null})),
        "events.read" => {
            let journal_id = parse_id_field::<Id>(&params, "journalId")?;
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_EVENT_LIMIT);
            serde_json::to_value(node.read_journal(
                &journal_id,
                optional_u64_field(&params, "afterSeq")?,
                limit,
            )?)
            .map_err(NodeError::from)
        }
        "events.ack" => Ok(json!({
            "acknowledgedSeq": optional_u64_field(&params, "throughSeq")?
                .ok_or_else(|| NodeError::InvalidRequest("events.ack requires throughSeq".to_owned()))?,
        })),
        "events.unsubscribe" | "tty.detach" => Ok(Value::Null),
        "tty.screen" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            node.screen_read(&instance_id).await
        }
        "tty.resize" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            let cols = params.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
            let rows = params.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
            let (cols, rows) = node.tty().resize(&instance_id, cols, rows).await?;
            Ok(json!({ "resizeRevision": "1", "cols": cols, "rows": rows }))
        }
        "tty.write" | "instance.keys" => {
            let instance_id = parse_id_field::<InstanceId>(&params, "instanceId")?;
            if let Some(raw) = params.get("dataBase64").and_then(Value::as_str)
                && !raw.is_empty()
            {
                let bytes = decode_data_base64(raw)?;
                node.tty().write_bytes(&instance_id, &bytes).await?;
                return Ok(json!({ "ok": true, "accepted": "tty-bytes" }));
            }
            let keys: Vec<String> = params
                .get("keys")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .filter(|item| !item.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if keys.is_empty() {
                return Err(NodeError::InvalidRequest(
                    "tty.write requires keys or dataBase64".to_owned(),
                ));
            }
            let bytes = remuda_driver::logical_keys_to_bytes(&keys);
            if node.tty().write_bytes(&instance_id, &bytes).await.is_ok() {
                return Ok(json!({ "ok": true, "accepted": "tty-bytes" }));
            }
            let result = node
                .submit_command(
                    &instance_id,
                    InstanceCommandRequest {
                        origin: remuda_protocol::InputOrigin::Human,
                        command_id: optional_id_field(&params, "commandId")?,
                        operation: CommandAction::WriteTty,
                        prompt: None,
                        prompt_mode: None,
                        attachments: Vec::new(),
                        run_id: None,
                        interaction_id: None,
                        answer: None,
                        keys: Some(keys),
                        model: None,
                        effort_name: None,
                        effort_index: None,
                        permission_mode: None,
                    },
                )
                .await?;
            serde_json::to_value(result).map_err(NodeError::from)
        }
        _ => Err(NodeError::InvalidRequest(format!(
            "JSON-RPC method is not implemented by remuda dev: {method}"
        ))),
    }
}

fn prompt_from_params(params: &Value) -> Option<String> {
    if let Some(text) = params
        .get("input")
        .and_then(|input| input.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    if let Some(blocks) = params
        .get("input")
        .and_then(|input| input.get("blocks"))
        .and_then(Value::as_array)
    {
        let mut out = String::new();
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    params
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

async fn submit_rpc_command(
    node: &DevNode,
    params: &Value,
    instance_id: InstanceId,
    operation: CommandAction,
    prompt: Option<String>,
) -> Result<Value, NodeError> {
    let result = node
        .submit_command(
            &instance_id,
            InstanceCommandRequest {
                origin: remuda_protocol::InputOrigin::Human,
                command_id: optional_id_field(params, "commandId")?,
                operation,
                // The local development REST surface stages no attachments;
                // those arrive via the Hub (D-027).
                attachments: Vec::new(),
                prompt,
                prompt_mode: None,
                run_id: optional_id_field(params, "runId")?,
                interaction_id: None,
                answer: None,
                keys: None,
                model: None,
                effort_name: None,
                effort_index: None,
                permission_mode: None,
            },
        )
        .await?;
    serde_json::to_value(result).map_err(NodeError::from)
}

fn runtime_hello(params: &Value, connection_id: Option<&Id>) -> Result<Value, NodeError> {
    let major = params
        .get("protocol")
        .and_then(|protocol| protocol.get("major"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if major != 1 {
        return Err(NodeError::InvalidRequest(format!(
            "unsupported protocol major {major}"
        )));
    }
    let connection_id = match connection_id {
        Some(value) => value.clone(),
        None => Id::new("epoch")?,
    };
    Ok(json!({
        "protocol": {"major": 1, "minor": 0},
        "connectionId": connection_id,
        "serverEpoch": Id::new("epoch")?,
        "observationSchemaMajor": 1,
        "features": ["snapshot-follow-v1", "tty-binary-v1", "local-dev-v1"],
        "limits": {
            "maxJsonFrameBytes": MAX_JSON_FRAME_BYTES,
            "maxBinaryChunkBytes": 65536,
            "maxTtyInputBytes": 4096,
            "maxInFlightRpc": 64,
            "maxEventsPerBatch": 128,
            "maxSubscriptionBufferEvents": 256,
            "heartbeatIntervalMs": 15000,
            "leaseTtlMs": 45000,
            "maxWaitMs": 30000
        },
        "reconcileRequired": false
    }))
}

async fn tty_attach(node: &DevNode, params: &Value) -> Result<(Value, Vec<Vec<u8>>), NodeError> {
    let instance_id = parse_id_field::<InstanceId>(params, "instanceId")?;
    let instance = node.get_instance(&instance_id)?;
    if let Some(requested_generation) = optional_u64_field(params, "processGeneration")?
        && requested_generation != instance.process_ref.process_generation
    {
        return Err(NodeError::Conflict("stale process generation".to_owned()));
    }
    let attached = node.tty().attach(&instance_id).await?;
    let mut frames = Vec::new();
    if !attached.snapshot.is_empty() {
        frames.push(encode_tty_frame(
            &attached.stream_id,
            attached.available_from,
            &attached.snapshot,
        )?);
    }
    Ok((attached.into_json()?, frames))
}

fn parse_instance_id(raw: &str) -> ApiResult<InstanceId> {
    InstanceId::from_str(raw)
        .map_err(NodeError::from)
        .map_err(ApiError)
}

fn parse_id_field<T>(params: &Value, name: &str) -> Result<T, NodeError>
where
    T: FromStr,
    NodeError: From<T::Err>,
{
    let raw = params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::InvalidRequest(format!("missing {name}")))?;
    T::from_str(raw).map_err(NodeError::from)
}

fn decode_data_base64(raw: &str) -> Result<Vec<u8>, NodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(raw.as_bytes())
        .map_err(|error| NodeError::InvalidRequest(format!("invalid dataBase64: {error}")))
}

fn optional_id_field<T>(params: &Value, name: &str) -> Result<Option<T>, NodeError>
where
    T: FromStr,
    NodeError: From<T::Err>,
{
    params
        .get(name)
        .filter(|value| !value.is_null())
        .map(|value| {
            let raw = value
                .as_str()
                .ok_or_else(|| NodeError::InvalidRequest(format!("{name} must be a string")))?;
            T::from_str(raw).map_err(NodeError::from)
        })
        .transpose()
}

fn optional_u64_field(params: &Value, name: &str) -> Result<Option<U64>, NodeError> {
    params
        .get(name)
        .filter(|value| !value.is_null())
        .map(|value| {
            if let Some(raw) = value.as_str() {
                let parsed = raw.parse::<u64>().map_err(|error| {
                    NodeError::InvalidRequest(format!("{name} is not a canonical U64: {error}"))
                })?;
                if parsed.to_string() != raw {
                    return Err(NodeError::InvalidRequest(format!(
                        "{name} is not a canonical U64"
                    )));
                }
                return Ok(U64(parsed));
            }
            value
                .as_u64()
                .map(U64)
                .ok_or_else(|| NodeError::InvalidRequest(format!("{name} must be a U64")))
        })
        .transpose()
}

async fn queue_json(outgoing: &mpsc::Sender<Message>, value: Value) -> Result<(), ()> {
    outgoing
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| ())
}

fn socket_error(message: String) -> Value {
    json!({"type": "error", "error": {"code": "local_error", "message": message}})
}

fn rpc_success(id: &str, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_failure(id: &str, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_node_failure(id: &str, error: NodeError) -> Value {
    let code = match error {
        NodeError::InvalidRequest(_) | NodeError::Wire(_) | NodeError::Json(_) => -32602,
        NodeError::NotFound { .. } => -32004,
        NodeError::Conflict(_) | NodeError::QueueFull => -32009,
        _ => -32603,
    };
    rpc_failure(id, code, &error.to_string())
}

type ApiResult<T> = Result<T, ApiError>;

struct ApiError(NodeError);

impl From<NodeError> for ApiError {
    fn from(error: NodeError) -> Self {
        Self(error)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(NodeError::Json(error))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            NodeError::NotFound { .. } => StatusCode::NOT_FOUND,
            NodeError::Conflict(_) => StatusCode::CONFLICT,
            NodeError::InvalidRequest(_) | NodeError::Wire(_) | NodeError::Json(_) => {
                StatusCode::BAD_REQUEST
            }
            NodeError::InteractionExpired => StatusCode::GONE,
            NodeError::InteractionSuperseded { .. } => StatusCode::CONFLICT,
            NodeError::QueueFull => StatusCode::TOO_MANY_REQUESTS,
            NodeError::InvalidConfig(_) => StatusCode::INTERNAL_SERVER_ERROR,
            NodeError::DriverUnavailable | NodeError::Driver(_) => StatusCode::SERVICE_UNAVAILABLE,
            NodeError::StorePoisoned | NodeError::Io(_) | NodeError::Sqlite(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            NodeError::Transport(_)
            | NodeError::HubRpc { .. }
            | NodeError::Disconnected
            | NodeError::JournalQueueClosed => StatusCode::BAD_GATEWAY,
        };
        api_response(status, "local_api_error", &self.0.to_string())
    }
}

fn api_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot/subscription boundary, which the gate's `tty_endpoint`
    /// failure exercises: `tty_socket` subscribes before it snapshots, so a
    /// frame can straddle the two and must be trimmed, never dropped and
    /// never replayed.
    #[test]
    fn a_live_frame_straddling_the_snapshot_is_trimmed_to_its_unseen_tail() {
        // Wholly after the snapshot: forwarded untouched.
        assert_eq!(live_tail(10, b"abc", 10), Some((10, &b"abc"[..])));
        assert_eq!(live_tail(12, b"abc", 10), Some((12, &b"abc"[..])));
        // Wholly inside it: the client already has these bytes.
        assert_eq!(live_tail(0, b"abc", 3), None);
        assert_eq!(live_tail(0, b"abc", 99), None);
        // Straddling: only the tail, anchored at the resume offset.
        assert_eq!(live_tail(8, b"abcde", 10), Some((10, &b"cde"[..])));
        // Empty payloads carry nothing either way.
        assert_eq!(live_tail(0, b"", 0), Some((0, &b""[..])));
    }

    #[test]
    fn access_code_comparison_and_cookie_token_are_deterministic() {
        assert!(constant_time_eq(b"correct", b"correct"));
        assert!(!constant_time_eq(b"correct", b"wrong"));
        assert_eq!(cookie_token("code"), cookie_token("code"));
        assert_ne!(cookie_token("code"), cookie_token("other"));
    }

    #[test]
    fn cookie_parser_does_not_accept_prefix_matches() {
        assert_eq!(
            cookie_value("other=1; remuda_dev_access_code=abc", ACCESS_CODE_COOKIE),
            Some("abc")
        );
        assert_eq!(
            cookie_value("xremuda_dev_access_code=abc", ACCESS_CODE_COOKIE),
            None
        );
    }
}
