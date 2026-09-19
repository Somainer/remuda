//! Proxy-host half of the relay (host H): rebuild the request against the
//! single pinned gateway origin, swap the credential, stream the response with
//! 16 KiB / 50 ms coalescing, and enforce the timeout ladder.
//!
//! Two front doors reach the same logic:
//! * [`serve_inbound`] drives one in-bound `api.open` stream (the path that
//!   always works, over the Hub↔Node link);
//! * [`direct_forward`] serves the optional direct-net listener with plain
//!   streaming HTTP (no frames, no coalescing).
//!
//! Security shape (§3): the destination origin is constructed from the
//! configured base URL — never from frame data; suffix paths are validated;
//! redirects are not followed, so a gateway cannot drag the credential onto
//! another origin; request/response headers cross only through the allowlists.

use super::policy::{filter_request_headers, filter_response_headers, relayed_path_is_safe};
use super::{InboundEvent, LinkBroker, Outbox, STREAM_HARD_CAP};
use crate::NodeError;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::StreamExt;
use remuda_protocol::hubnode::{
    API_ERROR_CANCELLED, API_ERROR_DESTINATION_REFUSED, API_ERROR_UPSTREAM_TIMEOUT, ApiChunkParams,
    ApiCreditParams, ApiEndError, ApiEndParams, ApiHeadParams, ApiHeader, ApiOpenParams,
    METHOD_API_CHUNK, METHOD_API_CREDIT, METHOD_API_END,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// TCP connect timeout (§7.6): 10 s.
pub(crate) const EGRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time to first response byte: 60 s.
pub(crate) const EGRESS_TTFT_TIMEOUT: Duration = Duration::from_secs(60);
/// Idle gap between response chunks: 120 s (SSE keepalives).
pub(crate) const EGRESS_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Response coalescing thresholds: flush at 16 KiB or 50 ms.
pub(crate) const COALESCE_BYTES: usize = 16 * 1024;
const COALESCE_INTERVAL: Duration = Duration::from_millis(50);
/// Hard ceiling on one relayed request body.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// Request header carrying the instance id on direct-net calls. It names the
/// egress context on H and is stripped before anything reaches the gateway.
pub(crate) const RELAY_INSTANCE_HEADER: &str = "x-remuda-relay-instance";

/// Which gateway credential the egress installs.
#[derive(Clone)]
pub enum CredentialKind {
    /// Gateway-style profile: `Authorization: Bearer …`
    /// (`ANTHROPIC_AUTH_TOKEN`).
    GatewayBearer(String),
    /// Direct-style profile: `x-api-key: …` (`ANTHROPIC_API_KEY`).
    ApiKey(String),
}

impl std::fmt::Debug for CredentialKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GatewayBearer(_) => formatter.write_str("GatewayBearer(<redacted>)"),
            Self::ApiKey(_) => formatter.write_str("ApiKey(<redacted>)"),
        }
    }
}

/// The timeout ladder, overridable for tests. Connect timeout is a property
/// of the shared [`reqwest::Client`] ([`EGRESS_CONNECT_TIMEOUT`]), not of a
/// single stream, so it is not per-context.
#[derive(Debug, Clone, Copy)]
pub struct EgressTimeouts {
    /// First response byte budget.
    pub ttft: Duration,
    /// Inter-chunk idle budget.
    pub idle: Duration,
    /// Whole-stream hard cap.
    pub hard: Duration,
}

impl Default for EgressTimeouts {
    fn default() -> Self {
        Self {
            ttft: EGRESS_TTFT_TIMEOUT,
            idle: EGRESS_IDLE_TIMEOUT,
            hard: STREAM_HARD_CAP,
        }
    }
}

/// Everything the proxy host needs to serve one instance's relay traffic.
///
/// Held in memory on H only; the credential is never persisted there (§6). The
/// Hub pushes a context before routing an `api.open` to this Node and removes
/// it when the instance ends.
#[derive(Debug, Clone)]
pub struct EgressContext {
    instance_id: String,
    base_url: url::Url,
    credential: CredentialKind,
    profile_headers: Vec<(String, String)>,
    timeouts: EgressTimeouts,
    coalesce_bytes: usize,
}

impl EgressContext {
    /// Build a context for `instance_id` pinned to `base_url`.
    ///
    /// `base_url` must be an absolute http(s) URL whose origin is the only
    /// origin requests may reach.
    pub fn new(
        instance_id: impl Into<String>,
        base_url: &str,
        credential: CredentialKind,
    ) -> Result<Arc<Self>, NodeError> {
        let base_url = url::Url::parse(base_url).map_err(|error| {
            NodeError::InvalidConfig(format!("egress baseUrl invalid: {error}"))
        })?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(NodeError::InvalidConfig(
                "egress baseUrl must be http(s)".into(),
            ));
        }
        Ok(Arc::new(Self {
            instance_id: instance_id.into(),
            base_url,
            credential,
            profile_headers: Vec::new(),
            timeouts: EgressTimeouts::default(),
            coalesce_bytes: COALESCE_BYTES,
        }))
    }

    /// Append the profile's configured extra headers (sent to the gateway on
    /// every request).
    pub fn with_profile_headers(
        mut self: Arc<Self>,
        headers: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Arc<Self> {
        let this = Arc::make_mut(&mut self);
        this.profile_headers = headers
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        self
    }

    /// Instance this context serves.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Path component of the pinned base URL (`""` or e.g. `/v1`).
    #[must_use]
    pub fn base_url_path(&self) -> &str {
        self.base_url.path()
    }

    /// Override coalescing threshold and the timeout ladder (tests).
    pub fn with_test_tuning(
        mut self: Arc<Self>,
        coalesce_bytes: usize,
        timeouts: EgressTimeouts,
    ) -> Arc<Self> {
        let this = Arc::make_mut(&mut self);
        this.coalesce_bytes = coalesce_bytes;
        this.timeouts = timeouts;
        self
    }
}

/// Rebuild the gateway URL from the pinned base plus a validated suffix.
///
/// `path` is the `api.open` path (under the base path); `query` is the raw
/// query string. The origin is entirely the context's — a frame cannot name a
/// host.
fn build_url(ctx: &EgressContext, path: &str, query: &str) -> Result<url::Url, NodeError> {
    if !relayed_path_is_safe(path) {
        return Err(NodeError::InvalidRequest(
            "api relay path escapes the pinned base path".into(),
        ));
    }
    let mut url = ctx.base_url.clone();
    let base_path = ctx.base_url.path().trim_end_matches('/');
    let full_path = format!("{base_path}{path}");
    url.set_path(&full_path);
    url.set_query(if query.is_empty() { None } else { Some(query) });
    Ok(url)
}

/// Apply the request header allowlist, install the profile credential, then
/// add the profile's own headers.
fn build_headers(ctx: &EgressContext, incoming: Vec<ApiHeader>) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = filter_request_headers(
        incoming
            .into_iter()
            .map(|header| (header.name, header.value)),
    );
    match &ctx.credential {
        CredentialKind::GatewayBearer(token) => {
            headers.push(("authorization".into(), format!("Bearer {token}")));
        }
        CredentialKind::ApiKey(token) => {
            headers.push(("x-api-key".into(), token.clone()));
        }
    }
    headers.extend(ctx.profile_headers.iter().cloned());
    headers
}

/// A ready direct-net response the proxy listener streams back to W.
pub(crate) struct DirectResponse {
    status: reqwest::StatusCode,
    headers: Vec<(String, String)>,
    response: reqwest::Response,
}

impl DirectResponse {
    /// HTTP status from the gateway.
    #[must_use]
    pub(crate) fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    /// Allowlisted response headers.
    #[must_use]
    pub(crate) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Gateway response body byte stream.
    pub(crate) fn into_byte_stream(
        self,
    ) -> impl futures::Stream<Item = Result<Bytes, reqwest::Error>> {
        self.response.bytes_stream()
    }
}

/// Run one direct-net request (H's direct listener → gateway). Same origin
/// pin and credential swap as [`serve_inbound`], no frame layer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn direct_forward(
    ctx: &EgressContext,
    http: &reqwest::Client,
    method: &str,
    path: &str,
    query: &str,
    headers: Vec<ApiHeader>,
    body: Option<Bytes>,
) -> Result<DirectResponse, NodeError> {
    let url = build_url(ctx, path, query)?;
    let method = method
        .parse::<reqwest::Method>()
        .map_err(|error| NodeError::InvalidRequest(format!("method not allowed: {error}")))?;
    let mut request = http
        .request(method, url)
        .headers(header_map(&build_headers(ctx, headers)));
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = tokio::time::timeout(ctx.timeouts.ttft, request.send())
        .await
        .map_err(|_| {
            NodeError::Transport("api relay direct upstream timed out before first byte".into())
        })?
        .map_err(|error| NodeError::Transport(format!("api relay direct upstream: {error}")))?;
    Ok(DirectResponse {
        status: response.status(),
        headers: response_headers_from(&response),
        response,
    })
}

fn header_map(headers: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            map.append(name, value);
        }
    }
    map
}

fn response_headers_from(response: &reqwest::Response) -> Vec<(String, String)> {
    filter_response_headers(response.headers().iter().map(|(name, value)| {
        (
            name.as_str().to_owned(),
            value.to_str().unwrap_or("").to_owned(),
        )
    }))
}

/// Terminal outcome of one in-band egress.
struct Outcome {
    end: ApiEndParams,
}

fn error_end(
    stream_id: &str,
    started: Instant,
    bytes_up: u64,
    bytes_down: u64,
    code: &str,
    message: &str,
) -> Outcome {
    Outcome {
        end: ApiEndParams {
            stream_id: stream_id.to_owned(),
            error: Some(ApiEndError {
                code: code.into(),
                message: message.into(),
            }),
            bytes_up,
            bytes_down,
            ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        },
    }
}

/// Drive one inbound `api.open` as the proxy host. Sends `api.head`,
/// `api.chunk` and a terminal `api.end` on `outbox`; consumes `api.body`,
/// `api.credit` and `api.cancel` from `events`.
pub(crate) async fn serve_inbound(
    state: &super::ApiRelayState,
    broker: &Arc<LinkBroker>,
    open: ApiOpenParams,
    mut outbox: Outbox,
    events: tokio::sync::mpsc::Receiver<InboundEvent>,
) {
    let started = Instant::now();
    let outcome = run_egress(state, broker, &open, &mut outbox, events, started).await;
    let _ = outbox.send_notification(METHOD_API_END, &outcome.end).await;
    broker.close_stream(&open.stream_id);
}

async fn run_egress(
    state: &super::ApiRelayState,
    _broker: &Arc<LinkBroker>,
    open: &ApiOpenParams,
    outbox: &mut Outbox,
    events: tokio::sync::mpsc::Receiver<InboundEvent>,
    started: Instant,
) -> Outcome {
    let Some(ctx) = state.egress_context(&open.instance_id) else {
        return error_end(
            &open.stream_id,
            started,
            0,
            0,
            API_ERROR_DESTINATION_REFUSED,
            "no egress is authorized for this instance on the proxy host",
        );
    };
    let url = match build_url(&ctx, &open.path, &open.query) {
        Ok(url) => url,
        Err(_) => {
            return error_end(
                &open.stream_id,
                started,
                0,
                0,
                API_ERROR_DESTINATION_REFUSED,
                "relayed path is outside the pinned origin",
            );
        }
    };
    if !matches!(open.method.as_str(), "GET" | "POST") {
        return error_end(
            &open.stream_id,
            started,
            0,
            0,
            API_ERROR_DESTINATION_REFUSED,
            "method is not allowed",
        );
    }

    // The event router owns the receiver: request body bytes feed the upstream
    // pump, `api.credit` replenishes the response window, and `api.cancel`
    // aborts everything.
    let cancelled = Arc::new(Notify::new());
    let bytes_up = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let mut router = EventRouter {
        events,
        body_tx: Some(body_tx),
        outbox: outbox.clone(),
        cancelled: Arc::clone(&cancelled),
        bytes_up: Arc::clone(&bytes_up),
    };
    let router_task = tokio::spawn(async move { router.route().await });

    // Upstream request construction.
    let headers = build_headers(&ctx, open.headers.clone());
    let method = match open.method.parse::<reqwest::Method>() {
        Ok(method) => method,
        Err(_) => {
            router_task.abort();
            return error_end(
                &open.stream_id,
                started,
                0,
                0,
                API_ERROR_DESTINATION_REFUSED,
                "method is not allowed",
            );
        }
    };
    let mut request = state
        .http()
        .request(method, url.clone())
        .headers(header_map(&headers));
    if open.body_chunked {
        let stream = futures::stream::unfold(body_rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|bytes| (Ok::<_, std::io::Error>(bytes), rx))
        });
        request = request.body(reqwest::Body::wrap_stream(stream));
    } else if let Some(body_base64) = &open.body_base64 {
        match BASE64.decode(body_base64.as_bytes()) {
            Ok(bytes) => request = request.body(bytes),
            Err(_) => {
                router_task.abort();
                return error_end(
                    &open.stream_id,
                    started,
                    0,
                    0,
                    API_ERROR_DESTINATION_REFUSED,
                    "request body base64 was invalid",
                );
            }
        }
    }

    let response = {
        let send = request.send();
        tokio::pin!(send);
        tokio::select! {
            result = &mut send => result,
            _ = cancelled.notified() => {
                router_task.abort();
                return error_end(&open.stream_id, started, 0, 0, API_ERROR_CANCELLED, "cancelled");
            }
            _ = tokio::time::sleep(ctx.timeouts.ttft) => {
                router_task.abort();
                return error_end(
                    &open.stream_id, started, 0, 0,
                    API_ERROR_UPSTREAM_TIMEOUT, "timed out before first byte",
                );
            }
        }
    };
    let response = match response {
        Ok(response) => response,
        // A refused connection and the ladder's connect timeout both surface
        // here; redirects are disabled so a 3xx is an ordinary response, not an
        // error.
        Err(error) if error.is_connect() || error.is_timeout() => {
            router_task.abort();
            return error_end(
                &open.stream_id,
                started,
                0,
                0,
                API_ERROR_UPSTREAM_TIMEOUT,
                "could not reach the pinned gateway origin",
            );
        }
        Err(error) => {
            router_task.abort();
            return error_end(
                &open.stream_id,
                started,
                0,
                0,
                API_ERROR_UPSTREAM_TIMEOUT,
                &format!("gateway request failed: {error}").replace('"', "'"),
            );
        }
    };

    // Commit the response head.
    let status = response.status().as_u16();
    let head_headers = response_headers_from(&response)
        .into_iter()
        .map(|(name, value)| ApiHeader { name, value })
        .collect();
    if outbox
        .send_notification(
            "api.head",
            &ApiHeadParams {
                stream_id: open.stream_id.clone(),
                status,
                headers: head_headers,
            },
        )
        .await
        .is_err()
    {
        router_task.abort();
        return error_end(
            &open.stream_id,
            started,
            0,
            0,
            remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST,
            "link closed while sending head",
        );
    }

    // Stream and coalesce the body.
    let mut upstream = response.bytes_stream();
    let mut coalescer = Coalescer::new(ctx.coalesce_bytes);
    let mut tick = tokio::time::interval(COALESCE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // consume the immediate tick
    let mut seq: u32 = 0;
    let mut bytes_down: u64 = 0;
    let mut failed: Option<Outcome> = None;
    // The idle deadline is one persistent timer reset only when response bytes
    // actually arrive. Recreating it per loop iteration would reset it on every
    // 50 ms tick and make the idle timeout unreachable.
    let mut idle_deadline = tokio::time::Instant::now() + ctx.timeouts.idle;
    loop {
        if started.elapsed() >= ctx.timeouts.hard {
            failed = Some(error_end(
                &open.stream_id,
                started,
                0,
                bytes_down,
                API_ERROR_UPSTREAM_TIMEOUT,
                "stream hard cap reached",
            ));
            break;
        }
        tokio::select! {
            _ = cancelled.notified() => {
                failed = Some(error_end(
                    &open.stream_id, started, 0, bytes_down,
                    API_ERROR_CANCELLED, "cancelled",
                ));
                break;
            }
            _ = tick.tick() => {
                if let Some(chunk) = coalescer.flush() {
                    let len = chunk.len() as u64;
                    if let Err(error) = send_chunk(outbox, &open.stream_id, seq, chunk, false).await {
                        failed = Some(error_end(&open.stream_id, started, 0, bytes_down,
                            remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST, &error.to_string()));
                        break;
                    }
                    seq += 1;
                    bytes_down += len;
                    tokio::task::yield_now().await;
                }
            }
            item = upstream.next() => {
                match item {
                    Some(Ok(bytes)) => {
                        idle_deadline = tokio::time::Instant::now() + ctx.timeouts.idle;
                        for chunk in coalescer.push(bytes) {
                            let len = chunk.len() as u64;
                            if let Err(error) = send_chunk(outbox, &open.stream_id, seq, chunk, false).await {
                                failed = Some(error_end(&open.stream_id, started, 0, bytes_down,
                                    remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST, &error.to_string()));
                                break;
                            }
                            seq += 1;
                            bytes_down += len;
                            tokio::task::yield_now().await;
                        }
                        if failed.is_some() {
                            break;
                        }
                    }
                    Some(Err(_)) => {
                        failed = Some(error_end(
                            &open.stream_id, started, 0, bytes_down,
                            API_ERROR_UPSTREAM_TIMEOUT, "gateway connection dropped",
                        ));
                        break;
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                failed = Some(error_end(
                    &open.stream_id, started, 0, bytes_down,
                    API_ERROR_UPSTREAM_TIMEOUT, "idle timeout waiting for gateway bytes",
                ));
                break;
            }
        }
    }
    router_task.abort();
    let bytes_up = bytes_up.load(std::sync::atomic::Ordering::Relaxed);

    // On success, flush the tail and send the zero-byte `last` chunk so the
    // worker side closes its body exactly once.
    if failed.is_none() {
        if let Some(chunk) = coalescer.flush() {
            let len = chunk.len() as u64;
            if send_chunk(outbox, &open.stream_id, seq, chunk, false)
                .await
                .is_err()
            {
                failed = Some(error_end(
                    &open.stream_id,
                    started,
                    bytes_up,
                    bytes_down,
                    remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST,
                    "link closed",
                ));
            } else {
                bytes_down += len;
            }
        }
        if failed.is_none()
            && send_chunk(outbox, &open.stream_id, seq, Bytes::new(), true)
                .await
                .is_err()
        {
            failed = Some(error_end(
                &open.stream_id,
                started,
                bytes_up,
                bytes_down,
                remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST,
                "link closed",
            ));
        }
    }

    failed.unwrap_or(Outcome {
        end: ApiEndParams {
            stream_id: open.stream_id.clone(),
            error: None,
            bytes_up,
            bytes_down,
            ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        },
    })
}

async fn send_chunk(
    outbox: &mut Outbox,
    stream_id: &str,
    seq: u32,
    data: Bytes,
    last: bool,
) -> Result<(), NodeError> {
    outbox
        .send_gated(
            METHOD_API_CHUNK,
            &ApiChunkParams {
                stream_id: stream_id.to_owned(),
                seq,
                data_base64: BASE64.encode(data.as_ref()),
                last,
            },
        )
        .await
}

/// Routes inbound events while an egress is live: body bytes upstream,
/// response-window credits back to the producer, cancels to the abort notify.
struct EventRouter {
    events: tokio::sync::mpsc::Receiver<InboundEvent>,
    body_tx: Option<tokio::sync::mpsc::Sender<Bytes>>,
    outbox: Outbox,
    cancelled: Arc<Notify>,
    bytes_up: Arc<std::sync::atomic::AtomicU64>,
}

impl EventRouter {
    async fn route(&mut self) {
        use std::sync::atomic::Ordering;
        while let Some(event) = self.events.recv().await {
            match event {
                InboundEvent::Body(body) => {
                    let bytes = match BASE64.decode(body.data_base64.as_bytes()) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(_) => continue,
                    };
                    let total = self
                        .bytes_up
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed)
                        + bytes.len() as u64;
                    if total > MAX_REQUEST_BYTES as u64 {
                        self.cancelled.notify_one();
                        break;
                    }
                    if let Some(tx) = self.body_tx.as_ref()
                        && tx.send(bytes).await.is_err()
                    {
                        // Upstream pump gone (e.g. first byte failed); the
                        // main loop is already finishing.
                        break;
                    }
                    // One credit per drained request-body chunk.
                    let _ = self
                        .outbox
                        .send_notification(
                            METHOD_API_CREDIT,
                            &ApiCreditParams {
                                stream_id: self.outbox.stream_id().to_owned(),
                                chunks: 1,
                            },
                        )
                        .await;
                    if body.last {
                        self.body_tx = None;
                    }
                }
                InboundEvent::Credit(credit) => self.outbox.grant(credit.chunks),
                InboundEvent::Cancel(_) => {
                    self.cancelled.notify_one();
                    break;
                }
                InboundEvent::End(_) => break,
                InboundEvent::Head(_) | InboundEvent::Chunk(_) => {}
            }
        }
    }
}

/// Coalesce opaque response bytes at a size threshold; the 50 ms time flush is
/// driven by the egress loop calling [`Self::flush`] on its interval.
#[derive(Debug)]
pub(crate) struct Coalescer {
    buf: Vec<u8>,
    threshold: usize,
}

impl Coalescer {
    /// Construct with the flush threshold (`COALESCE_BYTES` in production).
    pub(crate) fn new(threshold: usize) -> Self {
        Self {
            buf: Vec::new(),
            threshold,
        }
    }

    /// Add bytes, returning zero or more threshold-sized chunks. A remainder
    /// under the threshold stays buffered for the next push or the time flush.
    pub(crate) fn push(&mut self, bytes: Bytes) -> Vec<Bytes> {
        self.buf.extend_from_slice(&bytes);
        let mut out = Vec::new();
        while self.buf.len() >= self.threshold {
            let tail = self.buf.split_off(self.threshold);
            let full = std::mem::replace(&mut self.buf, tail);
            out.push(Bytes::from(full));
        }
        out
    }

    /// Emit whatever is buffered (once per 50 ms tick or at stream end).
    pub(crate) fn flush(&mut self) -> Option<Bytes> {
        if self.buf.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut self.buf);
        Some(Bytes::from(bytes))
    }
}
