//! Per-instance HTTP listeners for the relay.
//!
//! * on the worker host W: one loopback-only listener bound to
//!   `127.0.0.1:0`, per instance, minted bearer, revoked at exit. Each request
//!   is either turned into `api.*` frames on the Hub link (hub-relay) or
//!   streamed straight to H's direct-net listener (direct-net);
//! * on the proxy host H: an optional listener bound only to the operator's
//!   configured `Host.relayBind`, serving the same egress over plain HTTP.
//!
//! Nothing here ever binds a non-loopback address by default, and the worker
//! listener cannot accept a non-loopback peer even if one somehow reaches it.

use super::egress::{self, RELAY_INSTANCE_HEADER};
use super::policy::{
    self, RelayBearer, bearer_token, normalize_base_path, path_is_within_base,
    relayed_path_is_safe, strip_base_prefix, validate_proxy_bind,
};
use super::{ApiRelayState, BodyChunk, STREAM_HARD_CAP, split_body_chunks};
use crate::NodeError;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::StreamExt;
use remuda_protocol::hubnode::{
    ApiBodyParams, ApiCancelParams, ApiHeader, ApiOpenParams, METHOD_API_BODY, METHOD_API_CANCEL,
    METHOD_API_OPEN,
};
use remuda_protocol::{ApiRouteKind, ApiRouteMode, ProviderDeliveryMode, RequestedApiRoute};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// Launch-time route-probe budget (Amendment A1): 3 s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// First-byte budget for a direct-net client call.
const DIRECT_TTFT: Duration = Duration::from_secs(60);
/// Inter-chunk idle budget for a direct-net response.
const DIRECT_IDLE: Duration = Duration::from_secs(120);
/// Largest request body the listener will buffer or stream through.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Request bodies larger than one wire chunk never ride `api.open` inline: a
/// single coalesced axum read of, say, 2 MiB would base64-expand past the
/// 1 MiB JSON frame cap. Everything above this is streamed as `api.body`.
const INLINE_BODY_BYTES: usize = 64 * 1024;

/// How a worker listener forwards accepted requests.
#[derive(Debug, Clone)]
enum WorkerMode {
    /// In-band over the Hub↔Node link as `api.*` frames.
    HubRelay,
    /// Direct network call to H's relay endpoint.
    DirectNet { endpoint: url::Url },
}

/// One live per-instance listener.
pub(crate) struct RelayInstance {
    instance_id: String,
    bearer: RelayBearer,
    /// Base path shared with the profile's `baseUrl` (`""` or e.g. `/v1`).
    base_path: String,
    role: RelayRole,
    state: Arc<ApiRelayState>,
    local_addr: SocketAddr,
    /// Proxy-listener peer rules; empty for a worker listener (loopback only).
    rules: Vec<policy::PeerRule>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    live: AtomicBool,
}

#[derive(Clone)]
enum RelayRole {
    Worker(WorkerMode),
    /// Proxy half (H's optional direct-net listener). Wired by the Hub relay
    /// session in api-routing task 2; exercised by in-crate tests today.
    #[allow(dead_code)]
    Proxy(Arc<egress::EgressContext>),
}

impl std::fmt::Debug for RelayInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayInstance")
            .field("instance_id", &self.instance_id)
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl RelayInstance {
    pub(crate) fn instance_id(&self) -> String {
        self.instance_id.clone()
    }

    /// Base URL written into the instance overlay as `ANTHROPIC_BASE_URL`:
    /// `http://127.0.0.1:<port><base path>`.
    pub(crate) fn base_url(&self) -> String {
        format!(
            "http://127.0.0.1:{port}{base}",
            port = self.local_addr.port(),
            base = normalize_base_path(&self.base_path),
        )
    }

    /// Per-instance bearer value written into the 0600 overlay.
    pub(crate) fn bearer_token(&self) -> String {
        self.bearer.encoded()
    }

    /// Bound address (loopback for every worker listener). Read by tests and
    /// by the Hub relay session wiring (task 2) for the direct-net endpoint.
    #[allow(dead_code)]
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop serving and mark the bearer revoked. Idempotent and synchronous so
    /// a worker-task termination guard can run it from a destructor.
    pub(crate) fn shutdown(&self) {
        self.live.store(false, Ordering::SeqCst);
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = stop.send(());
        }
    }
}

/// Result of launch-time route provisioning.
#[derive(Debug)]
pub(crate) struct Provision {
    /// The running listener.
    pub(crate) listener: Arc<RelayInstance>,
    /// The resolved, echoed route — never `auto`.
    pub(crate) kind: ApiRouteKind,
}

/// A provisioned route plus the values the settings overlay must carry.
#[derive(Debug)]
pub(crate) struct ProvisionedRoute {
    /// Overlay inputs (loopback URL + per-instance bearer).
    pub(crate) overlay: super::RelayOverlay,
    /// The observed route, echoed in the create result.
    pub(crate) observed: remuda_protocol::ApiRoute,
    /// Drop guard that revokes the listener unless the worker commits it.
    pub(crate) guard: super::ProvisionGuard,
}

/// Provision the relay for a create request, or return `None` for a direct
/// delivery. Validates the one shape the wire leaves to the projector: a
/// `via` request must name a host (D-047); a nameless `via` is a refusal,
/// never a silent direct launch (D-035).
pub(crate) async fn provision_for_request(
    state: &Arc<ApiRelayState>,
    instance_id: &str,
    request: &crate::CreateInstanceRequest,
) -> Result<Option<ProvisionedRoute>, NodeError> {
    let Some(route) = request.api_route.as_ref() else {
        return Ok(None);
    };
    if !matches!(route.mode, ProviderDeliveryMode::Via) {
        return Ok(None);
    }
    let Some(via_host_id) = route.via_host_id.clone() else {
        return Err(NodeError::InvalidRequest(
            "api route mode `via` requires a viaHostId".into(),
        ));
    };
    let profile_base_url = request
        .provider_overlay
        .as_ref()
        .and_then(|overlay| overlay.get("baseUrl"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            NodeError::InvalidRequest(
                "api relay launch needs the profile baseUrl to bind its base path".into(),
            )
        })?;
    let provision = provision_worker(
        state,
        instance_id,
        profile_base_url,
        route,
        request.api_relay_endpoint.as_deref(),
    )
    .await?;
    let overlay = super::RelayOverlay {
        base_url: provision.listener.base_url(),
        bearer: provision.listener.bearer_token(),
    };
    Ok(Some(ProvisionedRoute {
        overlay,
        observed: remuda_protocol::ApiRoute::via(via_host_id, None, provision.kind),
        guard: super::ProvisionGuard::new(Arc::clone(state), instance_id.to_owned()),
    }))
}

/// Start the worker-side relay for one instance, probing the direct path when
/// the request allows it. The choice is made once here and echoed back; a
/// later direct-net failure does not switch modes (Amendment A1).
pub(crate) async fn provision_worker(
    state: &Arc<ApiRelayState>,
    instance_id: &str,
    profile_base_url: &str,
    requested: &RequestedApiRoute,
    endpoint: Option<&str>,
) -> Result<Provision, NodeError> {
    provision_worker_with_bearer(
        state,
        instance_id,
        profile_base_url,
        requested,
        endpoint,
        RelayBearer::mint(),
    )
    .await
}

/// Same as [`provision_worker`] with a caller-supplied bearer. Production
/// mints; the launch-time direct-net probe and H's listener share this secret
/// through the Hub (Amendment A1), and a supplied bearer lets a test stand up
/// both ends of one instance.
pub(crate) async fn provision_worker_with_bearer(
    state: &Arc<ApiRelayState>,
    instance_id: &str,
    profile_base_url: &str,
    requested: &RequestedApiRoute,
    endpoint: Option<&str>,
    bearer: RelayBearer,
) -> Result<Provision, NodeError> {
    let parsed = url::Url::parse(profile_base_url).map_err(|error| {
        NodeError::InvalidRequest(format!("api relay profile baseUrl invalid: {error}"))
    })?;
    let base_path = parsed.path().to_owned();
    match requested.route {
        ApiRouteMode::HubRelay => {
            let listener =
                start_worker(state, instance_id, &base_path, WorkerMode::HubRelay, bearer).await?;
            Ok(Provision {
                listener,
                kind: ApiRouteKind::HubRelay,
            })
        }
        ApiRouteMode::DirectNet => {
            let Some(endpoint) = endpoint.filter(|value| !value.trim().is_empty()) else {
                return Err(NodeError::InvalidRequest(
                    "api-via-unreachable: direct-net route named but no relay endpoint is configured on the proxy host".into(),
                ));
            };
            if !probe_endpoint(state, endpoint, instance_id, &bearer.encoded()).await {
                return Err(NodeError::InvalidRequest(format!(
                    "api-via-unreachable: probe of proxy relay endpoint {endpoint} failed"
                )));
            }
            let url = url::Url::parse(endpoint).map_err(|error| {
                NodeError::InvalidRequest(format!("relay endpoint invalid: {error}"))
            })?;
            let listener = start_worker(
                state,
                instance_id,
                &base_path,
                WorkerMode::DirectNet { endpoint: url },
                bearer,
            )
            .await?;
            Ok(Provision {
                listener,
                kind: ApiRouteKind::DirectNet,
            })
        }
        ApiRouteMode::Auto => {
            // Default to the path that always works; only a successful probe of
            // an explicitly-named endpoint promotes the route to direct-net.
            if let Some(endpoint) = endpoint.filter(|value| !value.trim().is_empty())
                && probe_endpoint(state, endpoint, instance_id, &bearer.encoded()).await
            {
                let url = url::Url::parse(endpoint).map_err(|error| {
                    NodeError::InvalidRequest(format!("relay endpoint invalid: {error}"))
                })?;
                let listener = start_worker(
                    state,
                    instance_id,
                    &base_path,
                    WorkerMode::DirectNet { endpoint: url },
                    bearer,
                )
                .await?;
                return Ok(Provision {
                    listener,
                    kind: ApiRouteKind::DirectNet,
                });
            }
            let listener =
                start_worker(state, instance_id, &base_path, WorkerMode::HubRelay, bearer).await?;
            Ok(Provision {
                listener,
                kind: ApiRouteKind::HubRelay,
            })
        }
    }
}

/// Probe H's direct-net listener: HEAD with the shared instance bearer, 3 s
/// budget. Any failure (bad URL, refused connection, bad status) reads as
/// "direct path unavailable"; the caller falls back or refuses.
pub(crate) async fn probe_endpoint(
    state: &ApiRelayState,
    endpoint: &str,
    instance_id: &str,
    bearer: &str,
) -> bool {
    let Ok(mut url) = url::Url::parse(endpoint) else {
        return false;
    };
    url.set_path("/");
    url.set_query(None);
    let request = state
        .http()
        .request(Method::HEAD, url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(RELAY_INSTANCE_HEADER, instance_id)
        .timeout(PROBE_TIMEOUT)
        .build();
    match request {
        Ok(request) => matches!(
            state.http().execute(request).await,
            Ok(response) if response.status().is_success()
        ),
        Err(_) => false,
    }
}

/// Bind and serve the worker-side listener on loopback only.
async fn start_worker(
    state: &Arc<ApiRelayState>,
    instance_id: &str,
    base_path: &str,
    mode: WorkerMode,
    bearer: RelayBearer,
) -> Result<Arc<RelayInstance>, NodeError> {
    // The only bind a worker listener ever performs. The `0` asks the kernel
    // for a free port; the `127.0.0.1` is non-negotiable.
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|error| {
        NodeError::Transport(format!("cannot bind api relay listener: {error}"))
    })?;
    let local_addr = listener.local_addr().map_err(|error| {
        NodeError::Transport(format!("api relay listener has no local addr: {error}"))
    })?;
    debug_assert!(local_addr.ip().is_loopback());
    start_serving(
        state,
        listener,
        local_addr,
        instance_id,
        base_path,
        RelayRole::Worker(mode),
        bearer,
        Vec::new(),
    )
    .await
}

/// Bind and serve the proxy-side direct-net listener at an operator-configured
/// `Host.relayBind`. Never called without one; bind policy refuses any-address
/// and public addresses. Production Hub-session wiring lands with api-routing
/// task 2; in-crate tests exercise it today.
#[allow(dead_code)]
pub(crate) async fn start_proxy(
    state: &Arc<ApiRelayState>,
    instance_id: &str,
    bind: &remuda_protocol::HostRelayBind,
    context: Arc<egress::EgressContext>,
    bearer: RelayBearer,
) -> Result<Arc<RelayInstance>, NodeError> {
    let socket = validate_proxy_bind(&bind.addr)?;
    let listener = TcpListener::bind(socket).await.map_err(|error| {
        NodeError::Transport(format!("cannot bind relayBind {socket}: {error}"))
    })?;
    let local_addr = listener.local_addr().map_err(|error| {
        NodeError::Transport(format!("relay listener has no local addr: {error}"))
    })?;
    let rules = bind
        .allow_from
        .iter()
        .map(|raw| policy::PeerRule::parse(raw))
        .collect::<Result<Vec<_>, _>>()?;
    let base_path = context.base_url_path().to_owned();
    start_serving(
        state,
        listener,
        local_addr,
        instance_id,
        &base_path,
        RelayRole::Proxy(context),
        bearer,
        rules,
    )
    .await
}

async fn start_serving(
    state: &Arc<ApiRelayState>,
    listener: TcpListener,
    local_addr: SocketAddr,
    instance_id: &str,
    base_path: &str,
    role: RelayRole,
    bearer: RelayBearer,
    rules: Vec<policy::PeerRule>,
) -> Result<Arc<RelayInstance>, NodeError> {
    let (stop, stopped) = oneshot::channel::<()>();
    let instance = Arc::new(RelayInstance {
        instance_id: instance_id.to_owned(),
        bearer,
        base_path: base_path.to_owned(),
        role,
        state: Arc::clone(state),
        local_addr,
        rules,
        stop: Mutex::new(Some(stop)),
        live: AtomicBool::new(true),
    });
    let app = axum::Router::new()
        .fallback(any(relay_handler))
        .with_state(Arc::clone(&instance));
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = stopped.await;
    });
    tokio::spawn(async move {
        if let Err(error) = server.await {
            tracing::debug!(%error, "api relay listener exited");
        }
    });
    state.register_instance(Arc::clone(&instance));
    Ok(instance)
}

// ── HTTP handling ──────────────────────────────────────────────────────────

/// Empty refusal: no detail is offered to a caller that failed policy.
fn refusal(status: StatusCode) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn anthropic_response(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    let body = policy::anthropic_error_body(code, message);
    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Fallback handler for every listener path: policy checks first, then a
/// role-specific relay.
async fn relay_handler(
    State(instance): State<Arc<RelayInstance>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> axum::response::Response {
    if !instance.live.load(Ordering::SeqCst) {
        return refusal(StatusCode::FORBIDDEN);
    }
    // Worker listeners carry no rules, so this rejects any non-loopback peer.
    // Proxy listeners additionally accept configured private ranges.
    if !policy::peer_allowed(peer.ip(), &instance.rules) {
        return refusal(StatusCode::FORBIDDEN);
    }
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Some(presented) = bearer_token(presented) else {
        return refusal(StatusCode::FORBIDDEN);
    };
    if !instance.bearer.verify(presented) {
        return refusal(StatusCode::FORBIDDEN);
    }

    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();

    // The direct-net probe (HEAD /) succeeds on the proxy listener once the
    // bearer is accepted, without touching the gateway.
    if matches!(instance.role, RelayRole::Proxy(_)) && parts.method == Method::HEAD {
        let named = parts
            .headers
            .get(RELAY_INSTANCE_HEADER)
            .and_then(|value| value.to_str().ok());
        return if named == Some(instance.instance_id.as_str()) {
            axum::response::Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        } else {
            refusal(StatusCode::FORBIDDEN)
        };
    }

    if !matches!(parts.method, Method::GET | Method::POST) {
        return refusal(StatusCode::NOT_FOUND);
    }
    if !path_is_within_base(&path, &instance.base_path) {
        return refusal(StatusCode::NOT_FOUND);
    }

    match &instance.role {
        RelayRole::Worker(WorkerMode::HubRelay) => {
            worker_inband(
                &instance,
                &parts.method,
                &path,
                parts.uri.query().unwrap_or(""),
                &parts.headers,
                body,
            )
            .await
        }
        RelayRole::Worker(WorkerMode::DirectNet { endpoint }) => {
            worker_direct(
                &instance,
                endpoint,
                &parts.method,
                &path,
                parts.uri.query().unwrap_or(""),
                &parts.headers,
                body,
            )
            .await
        }
        RelayRole::Proxy(ctx) => {
            proxy_direct(
                &instance,
                ctx,
                &parts.method,
                &path,
                parts.uri.query().unwrap_or(""),
                &parts.headers,
                body,
            )
            .await
        }
    }
}

/// Committed response head delivered from the in-band drive task.
struct ResponseHead {
    status: u16,
    headers: Vec<(String, String)>,
}

/// Why a stream ended before/around the response, carried to the listener.
#[derive(Debug)]
struct HeadFailure {
    code: String,
    message: String,
}

/// Worker side, hub-relay: turn one accepted HTTP request into `api.*` frames.
async fn worker_inband(
    instance: &Arc<RelayInstance>,
    method: &Method,
    path: &str,
    query: &str,
    headers: &axum::http::HeaderMap,
    body: Body,
) -> axum::response::Response {
    let Some(broker) = instance.state.active_link() else {
        return anthropic_response(
            StatusCode::SERVICE_UNAVAILABLE,
            remuda_protocol::hubnode::API_ERROR_VIA_HOST_OFFLINE,
            "no hub link for this relay instance",
        );
    };
    let Ok((stream_id, outbox, events)) = broker.open_worker_stream(&instance.instance_id) else {
        return anthropic_response(
            StatusCode::SERVICE_UNAVAILABLE,
            remuda_protocol::hubnode::API_ERROR_VIA_HOST_OFFLINE,
            "relay stream limit reached",
        );
    };

    // Decide inline vs chunked body from the first two body reads.
    let mut data_stream = body.into_data_stream();
    let body_mode = match read_body_opening(&mut data_stream).await {
        Ok(mode) => mode,
        Err(message) => {
            return anthropic_response(StatusCode::BAD_REQUEST, "bad-request", &message);
        }
    };

    let api_headers: Vec<ApiHeader> = policy::filter_request_headers(
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or(""))),
    )
    .into_iter()
    .map(|(name, value)| ApiHeader { name, value })
    .collect();
    let open = ApiOpenParams {
        instance_id: instance.instance_id.clone(),
        stream_id: stream_id.clone(),
        method: method.as_str().to_owned(),
        path: strip_base_prefix(path, &instance.base_path).to_owned(),
        query: query.to_owned(),
        headers: api_headers,
        body_base64: match &body_mode {
            BodyOpening::Inline(bytes) => Some(BASE64.encode(bytes)),
            _ => None,
        },
        body_chunked: matches!(body_mode, BodyOpening::Chunked { .. }),
        deadline_ms: STREAM_HARD_CAP.as_millis() as u32,
    };

    let (head_tx, head_rx) = oneshot::channel();
    let (down_tx, down_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let instance = Arc::clone(instance);
    tokio::spawn(async move {
        drive_inband(
            instance,
            outbox,
            events,
            open,
            body_mode,
            data_stream,
            head_tx,
            down_tx,
        )
        .await;
    });

    let head = match tokio::time::timeout(STREAM_HARD_CAP, head_rx).await {
        Ok(Ok(Ok(head))) => head,
        Ok(Ok(Err(failure))) => {
            let status = StatusCode::from_u16(policy::status_for_error(&failure.code))
                .unwrap_or(StatusCode::BAD_GATEWAY);
            return anthropic_response(status, &failure.code, &failure.message);
        }
        _ => {
            return anthropic_response(
                StatusCode::GATEWAY_TIMEOUT,
                remuda_protocol::hubnode::API_ERROR_UPSTREAM_TIMEOUT,
                "relay stream deadline elapsed before a response",
            );
        }
    };

    let mut builder = axum::response::Response::builder()
        .status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in &head.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let stream = futures::stream::unfold(down_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// What the opening reads of a request body decided.
enum BodyOpening {
    /// GET-style: no body.
    None,
    /// Whole body fit in the first read; rides `api.open.bodyBase64`.
    Inline(Bytes),
    /// More bytes follow as `api.body` frames.
    Chunked {
        /// Bytes already read (first chunk), excluding the peek that proved
        /// more data was coming.
        first: Bytes,
        peek: Bytes,
    },
}

async fn read_body_opening<S>(stream: &mut S) -> Result<BodyOpening, String>
where
    S: futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
{
    let Some(first_item) = stream.next().await else {
        return Ok(BodyOpening::None);
    };
    let first = first_item.map_err(|error| format!("request body read failed: {error}"))?;
    // A second read distinguishes "whole small body inline" from "stream it".
    // A first read already over the wire threshold streams even when no
    // further data arrives: the inline frame is the base64-expansion limit.
    if first.len() > INLINE_BODY_BYTES {
        return Ok(BodyOpening::Chunked {
            first,
            peek: Bytes::new(),
        });
    }
    match stream.next().await {
        None => Ok(BodyOpening::Inline(first)),
        Some(Ok(peek)) => {
            if (first.len() + peek.len()) > MAX_REQUEST_BYTES {
                return Err("request body too large".into());
            }
            Ok(BodyOpening::Chunked { first, peek })
        }
        Some(Err(error)) => Err(format!("request body read failed: {error}")),
    }
}

/// Drive one in-band stream after the listener opened it: request body up,
/// response events down, credits both ways, cancel on client disconnect.
#[allow(clippy::too_many_arguments)]
async fn drive_inband(
    instance: Arc<RelayInstance>,
    outbox: super::Outbox,
    mut events: mpsc::Receiver<super::InboundEvent>,
    open: ApiOpenParams,
    body_mode: BodyOpening,
    data_stream: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
    head_tx: oneshot::Sender<Result<ResponseHead, HeadFailure>>,
    down_tx: mpsc::Sender<BodyChunk>,
) {
    let mut head_tx = Some(head_tx);
    if outbox
        .send_notification(METHOD_API_OPEN, &open)
        .await
        .is_err()
    {
        let _ = head_tx.take().map(|tx| {
            tx.send(Err(HeadFailure {
                code: remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST.into(),
                message: "hub link closed".into(),
            }))
        });
        return;
    }

    // Request body upload (chunked case) runs concurrently with head/chunk
    // handling. It shares the outbox permit pool: `api.credit` frames arriving
    // below grant the upload's window through the shared `Arc<Semaphore>`.
    if let BodyOpening::Chunked { first, peek } = body_mode {
        let uploader = InbandUploader {
            stream_id: open.stream_id.clone(),
            outbox: outbox.clone(),
            buffered: vec![first, peek],
            data_stream,
        };
        tokio::spawn(uploader.run());
    }

    while let Some(event) = events.recv().await {
        match event {
            super::InboundEvent::Head(head) => {
                if let Some(tx) = head_tx.take() {
                    let _ = tx.send(Ok(ResponseHead {
                        status: head.status,
                        headers: policy::filter_response_headers(
                            head.headers
                                .into_iter()
                                .map(|header| (header.name, header.value)),
                        ),
                    }));
                }
            }
            super::InboundEvent::Chunk(chunk) => {
                let bytes = match BASE64.decode(chunk.data_base64.as_bytes()) {
                    Ok(bytes) => Bytes::from(bytes),
                    Err(_) => continue,
                };
                // A full/closed channel means the local HTTP client stopped
                // reading: cancel upstream and end.
                if down_tx.send(Ok(bytes)).await.is_err() {
                    let _ = outbox
                        .send_notification(
                            METHOD_API_CANCEL,
                            &ApiCancelParams {
                                stream_id: open.stream_id.clone(),
                                reason: "client disconnected".into(),
                            },
                        )
                        .await;
                    break;
                }
                // One credit per drained response chunk.
                let _ = outbox
                    .send_notification(
                        remuda_protocol::hubnode::METHOD_API_CREDIT,
                        &remuda_protocol::hubnode::ApiCreditParams {
                            stream_id: open.stream_id.clone(),
                            chunks: 1,
                        },
                    )
                    .await;
            }
            super::InboundEvent::Credit(credit) => outbox.grant(credit.chunks),
            super::InboundEvent::Cancel(cancel) => {
                let _ = down_tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        format!("relay cancelled: {}", cancel.reason),
                    )))
                    .await;
                break;
            }
            super::InboundEvent::End(end) => {
                if let Some(error) = end.error {
                    if let Some(tx) = head_tx.take() {
                        let _ = tx.send(Err(HeadFailure {
                            code: error.code,
                            message: error.message,
                        }));
                    } else {
                        let _ = down_tx
                            .send(Err(std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "relay ended with an error",
                            )))
                            .await;
                    }
                }
                break;
            }
            super::InboundEvent::Body(_) => {}
        }
    }
    let _ = instance;
}

/// Uploads a chunked request body as credit-gated `api.body` frames.
struct InbandUploader<S> {
    stream_id: String,
    outbox: super::Outbox,
    /// Bytes read before the upload started (in order).
    buffered: Vec<Bytes>,
    data_stream: S,
}

impl<S> InbandUploader<S>
where
    S: futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
{
    async fn run(mut self) {
        let buffered = std::mem::take(&mut self.buffered);
        let head = futures::stream::iter(buffered.into_iter().map(Ok::<Bytes, std::io::Error>));
        let tail = self.data_stream.map(|item| {
            item.map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string())
            })
        });
        let mut body = head.chain(tail);

        let mut seq: u32 = 0;
        let mut total: usize = 0;
        while let Some(item) = body.next().await {
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(_) => {
                    let _ = self
                        .outbox
                        .send_notification(
                            METHOD_API_CANCEL,
                            &ApiCancelParams {
                                stream_id: self.stream_id.clone(),
                                reason: "request body read failed".into(),
                            },
                        )
                        .await;
                    return;
                }
            };
            for piece in split_body_chunks(bytes, 65_536) {
                total = total.saturating_add(piece.len());
                if total > MAX_REQUEST_BYTES {
                    let _ = self
                        .outbox
                        .send_notification(
                            METHOD_API_CANCEL,
                            &ApiCancelParams {
                                stream_id: self.stream_id.clone(),
                                reason: "request body too large".into(),
                            },
                        )
                        .await;
                    return;
                }
                let frame = ApiBodyParams {
                    stream_id: self.stream_id.clone(),
                    seq,
                    data_base64: BASE64.encode(piece.as_ref()),
                    last: false,
                };
                if self
                    .outbox
                    .send_gated(METHOD_API_BODY, &frame)
                    .await
                    .is_err()
                {
                    return;
                }
                seq += 1;
            }
        }
        // Zero-byte terminator: the whole request body has been delivered.
        let last = ApiBodyParams {
            stream_id: self.stream_id.clone(),
            seq,
            data_base64: String::new(),
            last: true,
        };
        let _ = self.outbox.send_gated(METHOD_API_BODY, &last).await;
    }
}

/// Worker side, direct-net: forward the accepted request to H's relay listener
/// over ordinary HTTP, streaming both ways.
async fn worker_direct(
    instance: &Arc<RelayInstance>,
    endpoint: &url::Url,
    method: &Method,
    path: &str,
    query: &str,
    headers: &axum::http::HeaderMap,
    body: Body,
) -> axum::response::Response {
    let mut url = endpoint.clone();
    url.set_path(path);
    url.set_query(if query.is_empty() { None } else { Some(query) });

    let mut request = instance
        .state
        .http()
        .request(method.clone(), url)
        .bearer_auth(instance.bearer_token())
        .header(RELAY_INSTANCE_HEADER, &instance.instance_id);
    for (name, value) in policy::filter_request_headers(
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or(""))),
    ) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            request = request.header(name, value);
        }
    }
    let mapped = body.into_data_stream().map(|item| {
        item.map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string()))
    });
    request = request.body(reqwest::Body::wrap_stream(mapped));

    let response = match tokio::time::timeout(DIRECT_TTFT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) if error.is_timeout() => {
            return anthropic_response(
                StatusCode::GATEWAY_TIMEOUT,
                remuda_protocol::hubnode::API_ERROR_UPSTREAM_TIMEOUT,
                "direct relay timed out",
            );
        }
        Ok(Err(error)) => {
            return anthropic_response(
                StatusCode::SERVICE_UNAVAILABLE,
                remuda_protocol::hubnode::API_ERROR_VIA_HOST_OFFLINE,
                &format!("direct relay unavailable: {error}").replace('"', "'"),
            );
        }
        Err(_) => {
            return anthropic_response(
                StatusCode::GATEWAY_TIMEOUT,
                remuda_protocol::hubnode::API_ERROR_UPSTREAM_TIMEOUT,
                "direct relay timed out before first byte",
            );
        }
    };

    let status = response.status();
    let mut builder = axum::response::Response::builder().status(status);
    for (name, value) in policy::filter_response_headers(
        response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or(""))),
    ) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let upstream = response.bytes_stream();
    let stream = futures::stream::unfold(upstream, |mut upstream| async move {
        match tokio::time::timeout(DIRECT_IDLE, upstream.next()).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), upstream)),
            Ok(Some(Err(error))) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    error.to_string(),
                )),
                upstream,
            )),
            Ok(None) | Err(_) => None,
        }
    });
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Proxy side behind the optional direct-net listener: authorize, then run the
/// pinned-origin egress without a frame layer.
async fn proxy_direct(
    instance: &Arc<RelayInstance>,
    ctx: &Arc<egress::EgressContext>,
    method: &Method,
    path: &str,
    query: &str,
    headers: &axum::http::HeaderMap,
    body: Body,
) -> axum::response::Response {
    if headers
        .get(RELAY_INSTANCE_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some(instance.instance_id.as_str())
    {
        return refusal(StatusCode::FORBIDDEN);
    }
    let suffix = strip_base_prefix(path, &instance.base_path);
    if !relayed_path_is_safe(suffix) {
        return refusal(StatusCode::NOT_FOUND);
    }
    let mut collected = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                collected.extend_from_slice(&bytes);
                if collected.len() > MAX_REQUEST_BYTES {
                    return refusal(StatusCode::PAYLOAD_TOO_LARGE);
                }
            }
            Err(_) => return refusal(StatusCode::BAD_REQUEST),
        }
    }
    let api_headers: Vec<ApiHeader> = policy::filter_request_headers(
        headers
            .iter()
            .filter(|(name, _)| name.as_str() != RELAY_INSTANCE_HEADER)
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or(""))),
    )
    .into_iter()
    .map(|(name, value)| ApiHeader { name, value })
    .collect();
    let result = egress::direct_forward(
        ctx,
        instance.state.http(),
        method.as_str(),
        suffix,
        query,
        api_headers,
        Some(Bytes::from(collected)),
    )
    .await;
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            return anthropic_response(
                StatusCode::BAD_GATEWAY,
                remuda_protocol::hubnode::API_ERROR_UPSTREAM_TIMEOUT,
                &error.to_string(),
            );
        }
    };
    let mut builder = axum::response::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(response.into_byte_stream()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
