//! D-048 in-band model-API relay: the Hub-side `api.*` stream router.
//!
//! Two shapes of hop live here:
//!
//! * **Worker Node → proxy Node (`hub-relay` to another host).** The Hub is a
//!   pure frame switch: `api.open/body/credit/cancel` from the worker host are
//!   authorized against the instance, enriched with the profile's upstream
//!   snapshot (base URL, headers, credential) and forwarded to the proxy host;
//!   `api.head/chunk/end/cancel/credit` come back the other way. Every frame
//!   is a notification with its own stream table, so a hot stream never spends
//!   one of the 32 in-flight RPC slots.
//! * **Worker Node → the Hub process itself (`apiVia: self`).** The Hub is the
//!   proxy host: it does the outbound `reqwest` in-process, loads the gateway
//!   credential from the vault per stream, and emits `api.head/chunk/end`
//!   itself. The credential exists only in this task's memory.
//!
//! Either way the destination origin is pinned to the resolved profile's
//! `baseUrl`, request and response headers are allowlisted, and `api.end`
//! journals counters only — never a body or a header. There is no fallback to
//! direct delivery anywhere in this file: when the proxy leg fails, the stream
//! ends with an error and (for a dropped proxy link) the instance goes
//! `blocked{api-route-down}`.

use crate::AppState;
use crate::error::HubError;
use crate::store::StoreError;
use base64::Engine as _;
use remuda_protocol::hubnode::{
    ApiBodyParams, ApiChunkParams, ApiCreditParams, ApiEndError, ApiEndParams, ApiHeadParams,
    ApiOpenParams, METHOD_API_BODY, METHOD_API_CANCEL, METHOD_API_CHUNK, METHOD_API_CREDIT,
    METHOD_API_END, METHOD_API_HEAD, METHOD_API_OPEN,
};
use remuda_protocol::{
    API_ROUTE_DOWN,
    hubnode::{
        API_ERROR_CANCELLED, API_ERROR_DESTINATION_REFUSED, API_ERROR_HUB_LINK_LOST,
        API_ERROR_UPSTREAM_TIMEOUT, API_ERROR_VIA_HOST_OFFLINE,
    },
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

/// Convenience alias for the notification handlers: failures are delivered as
/// the stream's terminal frame, never a JSON-RPC error.
type RelayResult<T> = std::result::Result<T, String>;

/// Streams one live link may hold (protocol §7.6; `maxApiStreams` default 8).
const MAX_STREAMS_PER_LINK: usize = 8;
/// Streams one instance may own on a link (§7.6; 2/instance).
const MAX_STREAMS_PER_INSTANCE: usize = 2;
/// Initial producer window: at most this many chunks unacknowledged per leg.
const INITIAL_CHUNK_CREDITS: u32 = 4;
/// Connect timeout to the upstream gateway (§7.6).
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// First-byte timeout (§7.6).
const UPSTREAM_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(60);
/// Inter-chunk idle timeout; SSE keepalives are expected well inside it (§7.6).
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard cap for one relayed stream (§7.6).
const STREAM_HARD_CAP: Duration = Duration::from_secs(30 * 60);

/// Request headers the gateway may see. A cookie or proxy-auth header from the
/// harness must never be forwarded to the model origin; the worker's listener
/// strips them and the Hub re-checks.
pub const REQUEST_HEADER_ALLOWLIST: &[&str] = &[
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "accept-encoding",
    "user-agent",
];
/// Response headers the worker's listener may receive. `set-cookie` is dropped
/// no matter what the gateway answers.
pub const RESPONSE_HEADER_ALLOWLIST: &[&str] =
    &["content-type", "retry-after", "request-id", "x-request-id"];

/// The registry of every live relay stream in this Hub process.
#[derive(Clone, Default)]
pub struct ApiRelay {
    inner: Arc<Mutex<Registry>>,
}

#[derive(Default)]
struct Registry {
    /// Hub stream key → entry.
    streams: HashMap<String, Arc<Stream>>,
    /// (authenticated worker host, its stream id) → hub key.
    worker_legs: HashMap<(String, String), String>,
    /// (proxy host, hub-allocated stream id) → hub key.
    proxy_legs: HashMap<(String, String), String>,
}

/// One relayed request: two legs, one byte counter, one lifetime.
struct Stream {
    key: String,
    instance_id: String,
    /// Worker host W.
    worker_host: String,
    /// Stream id W minted.
    worker_stream: String,
    /// Proxy host H, or `None` when the Hub process itself is the proxy.
    proxy_host: Option<String>,
    /// Stream id used on the H leg (Hub-allocated; equal to `key`).
    proxy_stream: String,
    profile_id: String,
    /// Raw chunk cap from the link limits.
    chunk_limit: usize,
    started: Instant,
    state: Mutex<StreamState>,
    /// Request-body channel the in-process egress drains (chunked bodies only).
    /// `None` here after the terminal chunk closes the receiver's loop.
    body_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// H→W chunk window. Credits are tracked explicitly with an atomic counter
    /// (not a semaphore): the permit must stay held from *send* until W drains
    /// the frame, but the permit-drop happens on this same task when W credits,
    /// which a `SemaphorePermit` held across `.await` cannot express without a
    /// borrow conflict. The atomic mirrors that permit lifetime exactly.
    down_credits: Arc<std::sync::atomic::AtomicU32>,
    /// Set by `api.cancel` / `api.end` / link loss so the egress task stops.
    cancelled: AtomicBool,
}

#[derive(Default)]
struct StreamState {
    /// Response status from `api.head`, for the counter journal.
    status: Option<u16>,
    /// Request bytes seen (pre-base64).
    bytes_up: u64,
    /// Response bytes seen (post-base64).
    bytes_down: u64,
    /// Chunks sent W→H minus credits H returned.
    up_in_flight: u32,
    /// Chunks sent H→W minus credits W returned.
    down_in_flight: u32,
    /// Next expected request-body seq.
    expect_up_seq: u32,
    /// Next expected response-body seq.
    expect_down_seq: u32,
    /// Whether more request body is still coming.
    body_open: bool,
    /// Terminal frame already produced/forwarded.
    ended: bool,
}

impl ApiRelay {
    /// Process-wide registry constructor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one `api.*` notification from an authenticated Node link.
    ///
    /// Notification frames carry no RPC id and answer nothing: every failure is
    /// delivered as the terminal `api.end`/`api.cancel` of the affected stream,
    /// not as a JSON-RPC error to the sender.
    pub async fn handle_notification(
        &self,
        state: &AppState,
        host_id: &str,
        method: &str,
        params: Value,
    ) {
        let result: RelayResult<()> = match method {
            METHOD_API_OPEN => self.on_open(state, host_id, params).await,
            METHOD_API_BODY => {
                self.on_body(state, host_id, params).await;
                Ok(())
            }
            METHOD_API_HEAD => self.on_head(state, host_id, params).await,
            METHOD_API_CHUNK => self.on_chunk(state, host_id, params).await,
            METHOD_API_END => self.on_end(state, host_id, params).await,
            METHOD_API_CANCEL => self.on_cancel(state, host_id, params).await,
            METHOD_API_CREDIT => self.on_credit(state, host_id, params).await,
            other => {
                tracing::warn!(method = other, "unknown api.* notification");
                Ok(())
            }
        };
        if let Err(error) = result {
            tracing::warn!(%host_id, %method, error = %error, "api.* notification rejected");
        }
    }

    // ── worker leg: open ────────────────────────────────────────────────────

    async fn on_open(&self, state: &AppState, host_id: &str, params: Value) -> RelayResult<()> {
        let open: ApiOpenParams =
            serde_json::from_value(params).map_err(|error| format!("api.open params: {error}"))?;
        // Second authorization: the Node's local check is not sufficient, the
        // same way it is not for object.pull.
        let instance = state
            .store
            .get_instance(open.instance_id.clone())
            .await
            .map_err(|error| format!("load instance: {error}"))?
            .ok_or("api.open names an unknown instance")?;
        if instance.host_id != host_id {
            return Err("api.open host is not the instance's host".into());
        }
        let route = instance
            .api_route
            .as_ref()
            .filter(|route| route.is_via())
            .ok_or("instance has no via apiRoute; nothing to relay")?;
        // Direct-net sessions bypass the Hub entirely (W opens a TCP connection
        // to H's relayBind). An api.* frame for one here means a Node that
        // misread its own echo — a protocol error, never a reroute.
        if !matches!(route.route, Some(remuda_protocol::ApiRouteKind::HubRelay)) {
            return Err("api.open on a direct-net route must not traverse the Hub".into());
        }
        let profile_id = instance
            .provider_profile_id
            .as_deref()
            .filter(|id| crate::provider_resolve::is_real_profile_id(id))
            .ok_or("via instance names no gateway profile")?;
        let profile = state
            .store
            .get_provider(profile_id.to_string())
            .await
            .map_err(|error| format!("load profile: {error}"))?
            .ok_or("the instance's gateway profile vanished")?;
        // Re-check the pin against the pinned base URL, on every open. A
        // rejected open still has a listener waiting for api.end: deliver the
        // refusal to W, never a JSON-RPC error or silence.
        if let Err(message) = check_request_allowlist(&open.method, &open.path, &profile.base_url) {
            self.reject_unregistered_open(
                state,
                host_id,
                &open,
                API_ERROR_DESTINATION_REFUSED,
                message,
            )
            .await;
            return Ok(());
        }
        // The request-header allowlist: auth headers are stripped/swap targets,
        // the documented names (plus x-stainless-*) pass, anything else is a
        // refusal — the Hub re-checks what the listener was supposed to drop.
        for header in &open.headers {
            let name = header.name.to_ascii_lowercase();
            let allowed = matches!(name.as_str(), "authorization" | "x-api-key" | "api-key")
                || REQUEST_HEADER_ALLOWLIST
                    .iter()
                    .any(|allowed| name == *allowed)
                || name.starts_with("x-stainless-");
            if !allowed {
                self.reject_unregistered_open(
                    state,
                    host_id,
                    &open,
                    API_ERROR_DESTINATION_REFUSED,
                    format!("request header {name} is outside the relay allowlist"),
                )
                .await;
                return Ok(());
            }
        }

        let chunk_limit = state.config_relay_chunk_bytes();
        // Inline bodies count as up-bytes at open; chunked bodies count per
        // frame in `on_body`.
        let initial_bytes_up = match open.body_base64.as_deref() {
            Some(b64) => decode_b64(b64)?.len() as u64,
            None => 0,
        };
        let prepared = {
            let mut registry = self.inner.lock().await;
            let owned = registry
                .worker_legs
                .keys()
                .filter(|(host, _)| host == host_id)
                .count();
            if owned >= MAX_STREAMS_PER_LINK {
                self.reject_unregistered_open(
                    state,
                    host_id,
                    &open,
                    API_ERROR_DESTINATION_REFUSED,
                    "this link already holds maxApiStreams relay streams",
                )
                .await;
                return Ok(());
            }
            let for_instance = registry
                .worker_legs
                .values()
                .filter_map(|key| registry.streams.get(key))
                .filter(|stream| stream.instance_id == open.instance_id)
                .count();
            if for_instance >= MAX_STREAMS_PER_INSTANCE {
                self.reject_unregistered_open(
                    state,
                    host_id,
                    &open,
                    API_ERROR_DESTINATION_REFUSED,
                    "this instance already holds 2 relay streams",
                )
                .await;
                return Ok(());
            }
            if registry
                .worker_legs
                .contains_key(&(host_id.to_string(), open.stream_id.clone()))
            {
                return Err("duplicate streamId on this link".into());
            }
            let suffix = crate::config::new_id("obj").map_err(|error| error.to_string())?;
            let key = format!("{}-{}", open.stream_id, suffix);
            let body_channel = open.body_chunked.then(mpsc::unbounded_channel);
            let (body_tx, body_rx) = match body_channel {
                Some((tx, rx)) => (Some(tx), Some(rx)),
                None => (None, None),
            };
            let stream = Arc::new(Stream {
                key: key.clone(),
                instance_id: open.instance_id.clone(),
                worker_host: host_id.to_string(),
                worker_stream: open.stream_id.clone(),
                proxy_host: route.via_host_id.as_ref().map(|id| id.as_id().to_string()),
                proxy_stream: key.clone(),
                profile_id: profile.id.clone(),
                chunk_limit,
                started: Instant::now(),
                state: Mutex::new(StreamState {
                    body_open: open.body_chunked,
                    bytes_up: initial_bytes_up,
                    ..Default::default()
                }),
                body_tx: Mutex::new(body_tx),
                down_credits: Arc::new(std::sync::atomic::AtomicU32::new(INITIAL_CHUNK_CREDITS)),
                cancelled: AtomicBool::new(false),
            });
            registry
                .worker_legs
                .insert((host_id.to_string(), open.stream_id.clone()), key.clone());
            if let Some(proxy_host) = stream.proxy_host.as_ref() {
                registry
                    .proxy_legs
                    .insert((proxy_host.clone(), key.clone()), key.clone());
            }
            registry.streams.insert(key.clone(), stream.clone());
            PreparedOpen {
                stream,
                body_rx,
                profile,
                open,
            }
        };
        self.dispatch_open(state, prepared).await;
        Ok(())
    }

    /// Send a terminal `api.end` for an open that failed before the stream was
    /// registered. There is nothing to clean up, but W's listener is waiting.
    async fn reject_unregistered_open(
        &self,
        state: &AppState,
        host_id: &str,
        open: &ApiOpenParams,
        code: &str,
        message: impl Into<String>,
    ) {
        let params = json!({
            "streamId": open.stream_id,
            "error": { "code": code, "message": message.into() },
            "bytesUp": 0,
            "bytesDown": 0,
            "ms": 0,
        });
        let _ = state.nodes.notify(host_id, METHOD_API_END, params).await;
    }

    /// After registry bookkeeping: forward to H or run the in-process egress.
    ///
    /// Failures here end the stream to W rather than failing `api.open` (the
    /// open frame is a notification with no error channel).
    async fn dispatch_open(&self, state: &AppState, prepared: PreparedOpen) {
        let PreparedOpen {
            stream,
            body_rx,
            profile,
            open,
        } = prepared;
        if let Some(proxy_host) = stream.proxy_host.clone() {
            if !crate::provider_resolve::secret_release_allowed(&profile, &proxy_host) {
                self.end_to_worker(
                    state,
                    &stream,
                    Err(ApiEndError {
                        code: API_ERROR_DESTINATION_REFUSED.into(),
                        message: "the proxy host is outside the profile's secret scope".into(),
                    }),
                )
                .await;
                return;
            }
            let secret = match load_upstream_secret(state, &profile).await {
                Ok(secret) => secret,
                Err(error) => {
                    self.end_to_worker(
                        state,
                        &stream,
                        Err(ApiEndError {
                            code: API_ERROR_DESTINATION_REFUSED.into(),
                            message: format!("credential unavailable: {error}"),
                        }),
                    )
                    .await;
                    return;
                }
            };
            let mut params = match serde_json::to_value(&open) {
                Ok(value) => value,
                Err(error) => {
                    self.end_to_worker(
                        state,
                        &stream,
                        Err(ApiEndError {
                            code: API_ERROR_DESTINATION_REFUSED.into(),
                            message: format!("re-encode api.open: {error}"),
                        }),
                    )
                    .await;
                    return;
                }
            };
            if let Some(obj) = params.as_object_mut() {
                // The H leg addresses the stream by the Hub-allocated id.
                obj.insert("streamId".into(), json!(stream.proxy_stream));
                // The vault snapshot for this stream: the credential rides the
                // authenticated Hub↔H link and exists nowhere on W.
                obj.insert(
                    "upstream".into(),
                    json!({
                        "profileId": profile.id,
                        "baseUrl": profile.base_url,
                        "headers": profile.headers,
                        "authToken": secret,
                    }),
                );
            }
            let delivered = state
                .nodes
                .notify(&proxy_host, METHOD_API_OPEN, params)
                .await
                .unwrap_or(false);
            if !delivered {
                self.end_to_worker(
                    state,
                    &stream,
                    Err(ApiEndError {
                        code: API_ERROR_VIA_HOST_OFFLINE.into(),
                        message: "the proxy host's link dropped before api.open".into(),
                    }),
                )
                .await;
            }
        } else {
            // H is this Hub process: do the egress here.
            let secret = match load_upstream_secret(state, &profile).await {
                Ok(secret) => secret,
                Err(error) => {
                    self.end_to_worker(
                        state,
                        &stream,
                        Err(ApiEndError {
                            code: API_ERROR_DESTINATION_REFUSED.into(),
                            message: format!("credential unavailable: {error}"),
                        }),
                    )
                    .await;
                    return;
                }
            };
            let relay = self.clone();
            let state = state.clone();
            tokio::spawn(run_hub_egress(
                state,
                relay,
                stream,
                EgressCtx {
                    base_url: profile.base_url,
                    profile_headers: profile.headers,
                    secret,
                    open,
                    body_rx,
                },
            ));
        }
    }

    // ── worker leg: request body / credits / cancel / end ───────────────────

    async fn on_body(&self, state: &AppState, host_id: &str, params: Value) {
        let result = self.on_body_inner(state, host_id, params).await;
        if let Err((stream, code, message)) = result {
            // A notification cannot be answered with a JSON-RPC error: the
            // failure shows up as the stream's terminal api.end instead.
            stream.cancelled.store(true, Ordering::SeqCst);
            self.end_to_worker(
                state,
                &stream,
                Err(ApiEndError {
                    code: code.into(),
                    message,
                }),
            )
            .await;
        }
    }

    async fn on_body_inner(
        &self,
        state: &AppState,
        host_id: &str,
        params: Value,
    ) -> std::result::Result<(), (Arc<Stream>, &'static str, String)> {
        let body: ApiBodyParams = match serde_json::from_value(params) {
            Ok(body) => body,
            Err(error) => {
                // No stream identity without a parsed frame; the listener's
                // own timeout handles the orphan.
                tracing::warn!(%error, "unparseable api.body");
                return Ok(());
            }
        };
        let Some((stream, _)) = self.worker_stream(host_id, &body.stream_id).await else {
            // Unknown stream: nothing to terminate; the Node timed the open.
            return Ok(());
        };
        let fail = |message: String| (stream.clone(), API_ERROR_DESTINATION_REFUSED, message);
        let bytes = decode_b64(&body.data_base64).map_err(fail)?;
        let last = body.last;
        {
            let mut st = stream.state.lock().await;
            if body.seq != st.expect_up_seq {
                return Err(fail(format!(
                    "api.body seq {} out of order (expected {})",
                    body.seq, st.expect_up_seq
                )));
            }
            if bytes.len() > stream.chunk_limit {
                return Err(fail(format!(
                    "api.body chunk {} bytes exceeds apiChunkBytes {}",
                    bytes.len(),
                    stream.chunk_limit
                )));
            }
            if st.up_in_flight >= INITIAL_CHUNK_CREDITS {
                return Err(fail("api.body producer overran its credit window".into()));
            }
            st.expect_up_seq += 1;
            st.up_in_flight += 1;
            st.bytes_up = st.bytes_up.saturating_add(bytes.len() as u64);
            st.body_open = !last;
        }
        if let Some(proxy_host) = stream.proxy_host.as_deref() {
            let mut params = serde_json::to_value(&body)
                .map_err(|error| fail(format!("re-encode api.body: {error}")))?;
            params["streamId"] = json!(stream.proxy_stream);
            let delivered = state
                .nodes
                .notify(proxy_host, METHOD_API_BODY, params)
                .await
                .map_err(|error| fail(error.to_string()))?;
            if !delivered {
                return Err((
                    stream.clone(),
                    API_ERROR_VIA_HOST_OFFLINE,
                    "proxy host link lost during request body".into(),
                ));
            }
        } else {
            // In-process leg: feed the egress task. Dropping the stored
            // sender on the terminal chunk closes its receiver loop below.
            let mut body_tx = stream.body_tx.lock().await;
            if let Some(tx) = body_tx.as_ref() {
                if tx.send(bytes).is_err() {
                    stream.cancelled.store(true, Ordering::SeqCst);
                }
                if last {
                    *body_tx = None;
                }
            }
        }
        Ok(())
    }

    /// A `api.credit` frame adds producer window. W credits the chunks H
    /// produced; H credits the chunks W produced.
    async fn on_credit(&self, state: &AppState, host_id: &str, params: Value) -> RelayResult<()> {
        let credit: ApiCreditParams = serde_json::from_value(params)
            .map_err(|error| format!("api.credit params: {error}"))?;
        if let Some((stream, _)) = self.worker_stream(host_id, &credit.stream_id).await {
            {
                let mut st = stream.state.lock().await;
                st.down_in_flight = st
                    .down_in_flight
                    .saturating_sub(credit.chunks.min(st.down_in_flight));
            }
            // Only the in-process egress consumes this window; credits on a
            // stream forwarded to a proxy Node are passed through unchanged.
            if stream.proxy_host.is_none() {
                use std::sync::atomic::Ordering as AtomicOrdering;
                stream
                    .down_credits
                    .fetch_add(credit.chunks, AtomicOrdering::AcqRel);
            }
            if let Some(proxy_host) = stream.proxy_host.as_deref() {
                let mut params = serde_json::to_value(&credit)
                    .map_err(|error| format!("re-encode api.credit: {error}"))?;
                params["streamId"] = json!(stream.proxy_stream);
                let delivered = state
                    .nodes
                    .notify(proxy_host, METHOD_API_CREDIT, params)
                    .await
                    .map_err(|error| error.to_string())?;
                if !delivered {
                    self.end_to_worker(
                        state,
                        &stream,
                        Err(ApiEndError {
                            code: API_ERROR_VIA_HOST_OFFLINE.into(),
                            message: "proxy host link lost".into(),
                        }),
                    )
                    .await;
                }
            }
            return Ok(());
        }
        if let Some((stream, _)) = self.proxy_stream(host_id, &credit.stream_id).await {
            {
                let mut st = stream.state.lock().await;
                st.up_in_flight = st
                    .up_in_flight
                    .saturating_sub(credit.chunks.min(st.up_in_flight));
            }
            let mut params = serde_json::to_value(&credit)
                .map_err(|error| format!("re-encode api.credit: {error}"))?;
            params["streamId"] = json!(stream.worker_stream);
            let delivered = state
                .nodes
                .notify(&stream.worker_host, METHOD_API_CREDIT, params)
                .await
                .map_err(|error| error.to_string())?;
            if !delivered {
                self.cancel_upstream(state, &stream, "worker link gone")
                    .await;
            }
        }
        Ok(())
    }

    /// W abandoning the stream (client disconnect, listener error).
    async fn on_cancel(&self, state: &AppState, host_id: &str, params: Value) -> RelayResult<()> {
        let cancel: remuda_protocol::hubnode::ApiCancelParams = serde_json::from_value(params)
            .map_err(|error| format!("api.cancel params: {error}"))?;
        let Some((stream, _)) = self.worker_stream(host_id, &cancel.stream_id).await else {
            return Ok(());
        };
        stream.cancelled.store(true, Ordering::SeqCst);
        if let Some(proxy_host) = stream.proxy_host.as_deref() {
            let params = json!({
                "streamId": stream.proxy_stream,
                "reason": cancel.reason,
            });
            let _ = state
                .nodes
                .notify(proxy_host, METHOD_API_CANCEL, params)
                .await;
        }
        // No journal on a pure cancel: `api.end` is the auditable frame.
        self.remove(&stream.key).await;
        Ok(())
    }

    /// W ending its side early (rare; end normally comes from the proxy).
    async fn on_end(&self, state: &AppState, host_id: &str, params: Value) -> RelayResult<()> {
        let end: ApiEndParams =
            serde_json::from_value(params).map_err(|error| format!("api.end params: {error}"))?;
        if let Some((stream, _)) = self.proxy_stream(host_id, &end.stream_id).await {
            self.terminal_from_proxy(state, &stream, end).await;
            return Ok(());
        }
        if let Some((stream, _)) = self.worker_stream(host_id, &end.stream_id).await {
            stream.cancelled.store(true, Ordering::SeqCst);
            if let Some(proxy_host) = stream.proxy_host.as_deref() {
                let _ = state
                    .nodes
                    .notify(
                        proxy_host,
                        METHOD_API_CANCEL,
                        json!({
                            "streamId": stream.proxy_stream,
                            "reason": "worker ended the stream",
                        }),
                    )
                    .await;
            }
            self.remove(&stream.key).await;
        }
        Ok(())
    }

    // ── proxy leg: response head / chunks / end ─────────────────────────────

    async fn on_head(&self, state: &AppState, host_id: &str, params: Value) -> RelayResult<()> {
        let head: ApiHeadParams =
            serde_json::from_value(params).map_err(|error| format!("api.head params: {error}"))?;
        let Some((stream, _)) = self.proxy_stream(host_id, &head.stream_id).await else {
            return Err("api.head for an unknown stream".into());
        };
        {
            stream.state.lock().await.status = Some(head.status);
        }
        project_supply_status(state, &stream).await;
        let mut params =
            serde_json::to_value(&head).map_err(|error| format!("re-encode api.head: {error}"))?;
        params["streamId"] = json!(stream.worker_stream);
        let delivered = state
            .nodes
            .notify(&stream.worker_host, METHOD_API_HEAD, params)
            .await
            .map_err(|error| error.to_string())?;
        if !delivered {
            self.cancel_upstream(state, &stream, "worker link gone before api.head")
                .await;
        }
        Ok(())
    }

    async fn on_chunk(&self, state: &AppState, host_id: &str, params: Value) -> RelayResult<()> {
        let chunk: ApiChunkParams =
            serde_json::from_value(params).map_err(|error| format!("api.chunk params: {error}"))?;
        let Some((stream, _)) = self.proxy_stream(host_id, &chunk.stream_id).await else {
            return Err("api.chunk for an unknown stream".into());
        };
        let bytes = decode_b64(&chunk.data_base64)?;
        {
            let mut st = stream.state.lock().await;
            if chunk.seq != st.expect_down_seq {
                return Err(format!(
                    "api.chunk seq {} out of order (expected {})",
                    chunk.seq, st.expect_down_seq
                ));
            }
            if bytes.len() > stream.chunk_limit {
                return Err(format!(
                    "api.chunk chunk {} bytes exceeds apiChunkBytes {}",
                    bytes.len(),
                    stream.chunk_limit
                ));
            }
            if st.down_in_flight >= INITIAL_CHUNK_CREDITS {
                return Err("api.chunk producer overran its credit window".into());
            }
            st.expect_down_seq += 1;
            st.down_in_flight += 1;
            st.bytes_down = st.bytes_down.saturating_add(bytes.len() as u64);
        }
        let mut params = serde_json::to_value(&chunk)
            .map_err(|error| format!("re-encode api.chunk: {error}"))?;
        params["streamId"] = json!(stream.worker_stream);
        let delivered = state
            .nodes
            .notify(&stream.worker_host, METHOD_API_CHUNK, params)
            .await
            .map_err(|error| error.to_string())?;
        if !delivered {
            self.cancel_upstream(state, &stream, "worker link gone")
                .await;
        }
        Ok(())
    }

    /// Forward H's terminal frame to W, journal its counters, close.
    async fn terminal_from_proxy(&self, state: &AppState, stream: &Arc<Stream>, end: ApiEndParams) {
        let already = stream.state.lock().await.ended;
        if already {
            return;
        }
        stream.state.lock().await.ended = true;
        let status = stream.state.lock().await.status;
        journal_end(state, stream, status, end.error.as_ref()).await;
        let mut params = match serde_json::to_value(&end) {
            Ok(value) => value,
            Err(_) => return,
        };
        params["streamId"] = json!(stream.worker_stream);
        let _ = state
            .nodes
            .notify(&stream.worker_host, METHOD_API_END, params)
            .await;
        self.remove(&stream.key).await;
    }

    // ── link loss ───────────────────────────────────────────────────────────

    /// A Node link dropped.
    ///
    /// * Streams whose **proxy** host vanished end to W with
    ///   `via-host-offline`, and instances routing through that host go
    ///   `blocked{api-route-down}` — the request fails, it is never moved to
    ///   `hub-relay` mid-session (Amendment A1).
    /// * Streams whose **worker** host vanished are cancelled upstream so H (or
    ///   the in-process egress) stops reading the gateway.
    pub async fn on_link_lost(&self, state: &AppState, host_id: &str) {
        // Capture the stream handles inside the lock: `by_key` after removal
        // would find nothing.
        let (worker_streams, proxy_streams) = {
            let mut registry = self.inner.lock().await;
            let worker_keys = remove_matching(&mut registry.worker_legs, host_id);
            let proxy_keys = remove_matching(&mut registry.proxy_legs, host_id);
            let mut worker = Vec::new();
            let mut proxy = Vec::new();
            for key in worker_keys {
                if let Some(stream) = registry.streams.remove(&key) {
                    stream.cancelled.store(true, Ordering::SeqCst);
                    worker.push(stream);
                }
            }
            for key in proxy_keys {
                if let Some(stream) = registry.streams.remove(&key) {
                    stream.cancelled.store(true, Ordering::SeqCst);
                    proxy.push(stream);
                }
            }
            (worker, proxy)
        };
        for stream in &proxy_streams {
            self.end_to_worker(
                state,
                stream,
                Err(ApiEndError {
                    code: API_ERROR_VIA_HOST_OFFLINE.into(),
                    message: "the proxy host's Hub link dropped".into(),
                }),
            )
            .await;
        }
        if !proxy_streams.is_empty() {
            self.block_instances_via(state, host_id).await;
        }
        for stream in &worker_streams {
            if let Some(proxy_host) = stream.proxy_host.as_deref() {
                let _ = state
                    .nodes
                    .notify(
                        proxy_host,
                        METHOD_API_CANCEL,
                        json!({
                            "streamId": stream.proxy_stream,
                            "reason": "worker link lost",
                        }),
                    )
                    .await;
            }
        }
    }

    /// Mark in-progress workers on instances routed through the lost host as
    /// `blocked{api-route-down}`, with a Hub diagnostic on each instance.
    async fn block_instances_via(&self, state: &AppState, host_id: &str) {
        let instance_ids = match state.store.instances_routed_via(host_id.to_string()).await {
            Ok(ids) => ids,
            Err(error) => {
                tracing::error!(%host_id, %error, "cannot list instances routed via lost host");
                return;
            }
        };
        for instance_id in instance_ids {
            // Block in-progress workers directly by host + instance, not by
            // reading the row first: a dispatched worker may not have a screen
            // and so get skipped by classify, but the blocked state itself is
            // exactly what `remuda watch` reports.
            let Some(instance) = state
                .store
                .get_instance(instance_id.clone())
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            let workers = match state
                .store
                .list_workers_for_host(instance.host_id.clone())
                .await
            {
                Ok(workers) => workers,
                Err(_) => continue,
            };
            for worker in workers {
                if !worker.state.is_in_progress() {
                    continue;
                }
                if worker
                    .instance_id
                    .as_ref()
                    .is_none_or(|id| id.as_id().as_str() != instance_id)
                {
                    continue;
                }
                let _ = state
                    .store
                    .mutate_worker(worker.meta.id.as_id().to_string(), |row| {
                        row.meta.revision = remuda_protocol::U64(row.meta.revision.0 + 1);
                        row.meta.updated_at =
                            remuda_protocol::Timestamp::try_from(crate::config::now_rfc3339())
                                .map_err(|_| StoreError::Id("bad timestamp".into()))?;
                        row.state = remuda_protocol::WorkerState::Blocked {
                            reason: API_ROUTE_DOWN.into(),
                        };
                        Ok(())
                    })
                    .await;
            }
            crate::ws::publish_hub_diagnostic(
                state,
                &instance_id,
                "api_route_down",
                &format!("the proxy host {host_id} went away; the API route is down (no reroute)"),
            )
            .await;
        }
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    async fn worker_stream(&self, host_id: &str, stream_id: &str) -> Option<(Arc<Stream>, String)> {
        let registry = self.inner.lock().await;
        let key = registry
            .worker_legs
            .get(&(host_id.to_string(), stream_id.to_string()))?
            .clone();
        let stream = registry.streams.get(&key)?.clone();
        Some((stream, key))
    }

    async fn proxy_stream(&self, host_id: &str, stream_id: &str) -> Option<(Arc<Stream>, String)> {
        let registry = self.inner.lock().await;
        let key = registry
            .proxy_legs
            .get(&(host_id.to_string(), stream_id.to_string()))?
            .clone();
        let stream = registry.streams.get(&key)?.clone();
        Some((stream, key))
    }

    #[allow(dead_code)] // used by tests/future diagnostics
    async fn by_key(&self, key: &str) -> Option<Arc<Stream>> {
        self.inner.lock().await.streams.get(key).cloned()
    }

    async fn remove(&self, key: &str) {
        let mut registry = self.inner.lock().await;
        if let Some(stream) = registry.streams.remove(key) {
            registry
                .worker_legs
                .remove(&(stream.worker_host.clone(), stream.worker_stream.clone()));
            if let Some(proxy_host) = stream.proxy_host.as_ref() {
                registry
                    .proxy_legs
                    .remove(&(proxy_host.clone(), stream.proxy_stream.clone()));
            }
        }
    }

    /// Ask the proxy leg to abandon an upstream when W vanished.
    async fn cancel_upstream(&self, state: &AppState, stream: &Arc<Stream>, reason: &str) {
        stream.cancelled.store(true, Ordering::SeqCst);
        if let Some(proxy_host) = stream.proxy_host.as_deref() {
            let _ = state
                .nodes
                .notify(
                    proxy_host,
                    METHOD_API_CANCEL,
                    json!({ "streamId": stream.proxy_stream, "reason": reason }),
                )
                .await;
        }
        self.remove(&stream.key).await;
    }

    /// Send a terminal `api.end` to the worker leg and close the stream.
    async fn end_to_worker(
        &self,
        state: &AppState,
        stream: &Arc<Stream>,
        error: std::result::Result<(), ApiEndError>,
    ) {
        {
            let mut st = stream.state.lock().await;
            if st.ended {
                return;
            }
            st.ended = true;
        }
        let (status, bytes_up, bytes_down, ms) = {
            let st = stream.state.lock().await;
            (
                st.status,
                st.bytes_up,
                st.bytes_down,
                stream.started.elapsed().as_millis() as u64,
            )
        };
        let error_ref = error.err();
        journal_end(state, stream, status, error_ref.as_ref()).await;
        let mut params = json!({
            "streamId": stream.worker_stream,
            "bytesUp": bytes_up,
            "bytesDown": bytes_down,
            "ms": ms,
        });
        if let Some(error) = error_ref {
            params["error"] = serde_json::to_value(error).unwrap_or(Value::Null);
        }
        let _ = state
            .nodes
            .notify(&stream.worker_host, METHOD_API_END, params)
            .await;
        self.remove(&stream.key).await;
    }
}

/// Everything `dispatch_open` needs after the registry insert.
struct PreparedOpen {
    stream: Arc<Stream>,
    body_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    profile: crate::store::ProviderRecord,
    open: ApiOpenParams,
}

/// Collect and remove the stream keys of one host from a leg index.
fn remove_matching(index: &mut HashMap<(String, String), String>, host_id: &str) -> Vec<String> {
    let legs: Vec<(String, String)> = index
        .keys()
        .filter(|(host, _)| host == host_id)
        .cloned()
        .collect();
    let mut out = Vec::new();
    for leg in legs {
        if let Some(key) = index.remove(&leg)
            && !out.contains(&key)
        {
            out.push(key);
        }
    }
    out
}

// ── in-process egress (H == Hub host) ────────────────────────────────────────

/// What the in-process egress needs for one stream.
struct EgressCtx {
    base_url: String,
    profile_headers: std::collections::BTreeMap<String, String>,
    secret: Option<String>,
    open: ApiOpenParams,
    body_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
}

/// Perform one relayed request from this Hub process to the gateway.
async fn run_hub_egress(state: AppState, relay: ApiRelay, stream: Arc<Stream>, egress: EgressCtx) {
    let EgressCtx {
        base_url,
        profile_headers,
        secret,
        open,
        body_rx,
    } = egress;
    let result = drive_upstream(
        &state,
        &stream,
        &base_url,
        &profile_headers,
        secret,
        &open,
        body_rx,
    )
    .await;
    relay.end_to_worker(&state, &stream, result).await;
}

async fn drive_upstream(
    state: &AppState,
    stream: &Arc<Stream>,
    base_url: &str,
    profile_headers: &std::collections::BTreeMap<String, String>,
    secret: Option<String>,
    open: &ApiOpenParams,
    mut body_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
) -> std::result::Result<(), ApiEndError> {
    let client = reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .no_proxy()
        .build()
        .map_err(|error| ApiEndError {
            code: API_ERROR_UPSTREAM_TIMEOUT.into(),
            message: format!("client build: {error}"),
        })?;
    let url = pinned_url(base_url, &open.path, &open.query)?;

    // Buffer the full request body BEFORE building/sending the upstream
    // request. With an inline body this is instant; with `bodyChunked` the
    // channel only closes on the final api.body, and awaiting it here (before
    // any down-leg chunk can arrive) avoids the deadlock where an immediate
    // gateway response fills the bounded credit window while this task is still
    // parked awaiting a response whose body it has not begun to consume.
    let body_bytes = collect_request_body(open, body_rx.as_mut(), stream).await?;

    let method = match open.method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        other => {
            return Err(ApiEndError {
                code: API_ERROR_DESTINATION_REFUSED.into(),
                message: format!("method {other} is not allowlisted"),
            });
        }
    };
    let mut request = client.request(method, url);

    // Strip any credentials the harness attached and keep only allowlisted
    // request headers; the profile's credential is the one that goes out.
    for header in &open.headers {
        let name = header.name.to_ascii_lowercase();
        if matches!(name.as_str(), "authorization" | "x-api-key" | "api-key") {
            continue;
        }
        if REQUEST_HEADER_ALLOWLIST
            .iter()
            .any(|allowed| name == *allowed || name.starts_with("x-stainless-"))
        {
            request = request.header(&header.name, &header.value);
        }
    }
    if let Some(secret) = secret.as_deref().filter(|value| !value.is_empty()) {
        request = request
            .header("Authorization", format!("Bearer {secret}"))
            .header("x-api-key", secret)
            .header("anthropic-version", "2023-06-01");
    }
    for (name, value) in profile_headers {
        if !matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "x-api-key" | "api-key"
        ) {
            request = request.header(name, value);
        }
    }

    if !body_bytes.is_empty() {
        request = request.body(body_bytes);
    }

    let send = tokio::time::timeout(STREAM_HARD_CAP, request.send());
    let response = send
        .await
        .map_err(|_| ApiEndError {
            code: API_ERROR_UPSTREAM_TIMEOUT.into(),
            message: "stream hard cap reached before the gateway responded".into(),
        })?
        .map_err(|error| ApiEndError {
            code: if error.is_timeout() {
                API_ERROR_UPSTREAM_TIMEOUT
            } else {
                API_ERROR_DESTINATION_REFUSED
            }
            .into(),
            message: format!("upstream request: {error}"),
        })?;

    let status = response.status().as_u16();
    let response_headers: Vec<remuda_protocol::hubnode::ApiHeader> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let lower = name.as_str().to_ascii_lowercase();
            let allowed = RESPONSE_HEADER_ALLOWLIST
                .iter()
                .any(|allowed| lower == *allowed || lower.starts_with("anthropic-"));
            if !allowed {
                return None;
            }
            value
                .to_str()
                .ok()
                .map(|value| remuda_protocol::hubnode::ApiHeader {
                    name: name.as_str().to_string(),
                    value: value.to_string(),
                })
        })
        .collect();
    {
        stream.state.lock().await.status = Some(status);
    }
    let head_delivered = state
        .nodes
        .notify(
            &stream.worker_host,
            METHOD_API_HEAD,
            json!({
                "streamId": stream.worker_stream,
                "status": status,
                "headers": response_headers,
            }),
        )
        .await
        .map_err(|error| ApiEndError {
            code: API_ERROR_HUB_LINK_LOST.into(),
            message: error.to_string(),
        })?;
    if !head_delivered {
        return Err(ApiEndError {
            code: API_ERROR_HUB_LINK_LOST.into(),
            message: "worker link not writable for api.head".into(),
        });
    }
    project_supply_status_oneshot(state, stream, Some(status)).await;

    // Stream the body in chunk_limit frames, respecting the down-leg window.
    let mut response = response;
    let mut seq = 0u32;
    let mut leftover: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        let chunk = match tokio::time::timeout(
            if first {
                UPSTREAM_FIRST_BYTE_TIMEOUT
            } else {
                UPSTREAM_IDLE_TIMEOUT
            },
            response.chunk(),
        )
        .await
        {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(error)) => {
                return Err(ApiEndError {
                    code: API_ERROR_DESTINATION_REFUSED.into(),
                    message: format!("reading upstream body: {error}"),
                });
            }
            Err(_) => {
                return Err(ApiEndError {
                    code: API_ERROR_UPSTREAM_TIMEOUT.into(),
                    message: if first {
                        "no first byte within the 60s timeout"
                    } else {
                        "no upstream chunk within the 120s idle timeout"
                    }
                    .into(),
                });
            }
        };
        first = false;
        let Some(chunk) = chunk else {
            break;
        };
        let mut bytes = std::mem::take(&mut leftover);
        bytes.extend_from_slice(&chunk);
        // Emit each bounded frame one at a time, yielding after every send so
        // a buffered multi-megabyte upstream read cannot burst past the
        // producer window: `send_down_chunk` blocks once the 4-chunk credits
        // are outstanding, and the yield lets the credit-return future run
        // before the next slice is attempted.
        while bytes.len() > stream.chunk_limit {
            let frame: Vec<u8> = bytes.drain(..stream.chunk_limit).collect();
            send_down_chunk(state, stream, &mut seq, frame, false).await?;
            tokio::task::yield_now().await;
        }
        leftover = bytes;
    }
    if !leftover.is_empty() {
        let frame = std::mem::take(&mut leftover);
        send_down_chunk(state, stream, &mut seq, frame, false).await?;
    }
    // The empty terminal chunk carries last=true the same way the Node egress
    // marks its final frame.
    send_down_chunk(state, stream, &mut seq, Vec::new(), true).await?;
    Ok(())
}

async fn send_down_chunk(
    state: &AppState,
    stream: &Arc<Stream>,
    seq: &mut u32,
    data: Vec<u8>,
    last: bool,
) -> std::result::Result<(), ApiEndError> {
    // Window: at most INITIAL_CHUNK_CREDITS chunks ahead of the consumer.
    // Claim a credit atomically (never going negative); if none are left, park
    // until W's api.credit tops the counter back up.
    use std::sync::atomic::Ordering as AtomicOrdering;
    loop {
        let current = stream.down_credits.load(AtomicOrdering::Acquire);
        if current == 0
            || stream
                .down_credits
                .compare_exchange_weak(
                    current,
                    current - 1,
                    AtomicOrdering::AcqRel,
                    AtomicOrdering::Acquire,
                )
                .is_ok()
        {
            if current > 0 {
                break;
            }
            if stream.cancelled.load(AtomicOrdering::SeqCst) {
                return Err(ApiEndError {
                    code: API_ERROR_CANCELLED.into(),
                    message: "credit window closed".into(),
                });
            }
            // No credit available; wait to be woken by on_credit.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    {
        let mut st = stream.state.lock().await;
        if st.ended || stream.cancelled.load(Ordering::SeqCst) {
            return Err(ApiEndError {
                code: API_ERROR_CANCELLED.into(),
                message: "stream ended".into(),
            });
        }
        st.bytes_down = st.bytes_down.saturating_add(data.len() as u64);
        st.down_in_flight += 1;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    let delivered = state
        .nodes
        .notify(
            &stream.worker_host,
            METHOD_API_CHUNK,
            json!({
                "streamId": stream.worker_stream,
                "seq": *seq,
                "dataBase64": encoded,
                "last": last,
            }),
        )
        .await
        .map_err(|error| ApiEndError {
            code: API_ERROR_HUB_LINK_LOST.into(),
            message: error.to_string(),
        })?;
    if !delivered {
        return Err(ApiEndError {
            code: API_ERROR_HUB_LINK_LOST.into(),
            message: "worker link not writable".into(),
        });
    }
    *seq += 1;
    Ok(())
}

/// Assemble the request body. Inline when it fit one frame; channel-fed and
/// bounded by the stream hard cap otherwise.
async fn collect_request_body(
    open: &ApiOpenParams,
    body_rx: Option<&mut mpsc::UnboundedReceiver<Vec<u8>>>,
    stream: &Arc<Stream>,
) -> std::result::Result<Vec<u8>, ApiEndError> {
    if let Some(b64) = open.body_base64.as_ref() {
        return decode_b64(b64).map_err(|_| ApiEndError {
            code: API_ERROR_DESTINATION_REFUSED.into(),
            message: "api.open bodyBase64 is not valid base64".into(),
        });
    }
    let Some(rx) = body_rx else {
        return Ok(Vec::new());
    };
    let mut body = Vec::new();
    let deadline = tokio::time::Instant::now() + STREAM_HARD_CAP;
    // The listener sends the terminal body chunk then drops its sender; until
    // then every channel read is another in-order chunk.
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        body.extend_from_slice(&chunk);
        if stream.cancelled.load(Ordering::SeqCst) {
            return Err(ApiEndError {
                code: API_ERROR_CANCELLED.into(),
                message: "stream cancelled".into(),
            });
        }
    }
    Ok(body)
}

// ── validation helpers ───────────────────────────────────────────────────────

/// Method/path allowlist plus the origin pin. The Node enforces this on its
/// listener; the Hub re-checks before it touches a credential.
pub fn check_request_allowlist(method: &str, path: &str, base_url: &str) -> RelayResult<()> {
    if !matches!(method, "GET" | "POST") {
        return Err(format!("method {method} is not allowlisted"));
    }
    // Reuse the pin itself; it carries every path/traversal/origin rule.
    let _ = pinned_url(base_url, path, "").map_err(|error| error.message)?;
    Ok(())
}

/// Join the relayed path under the profile base URL, refusing anything that
/// escapes the pinned origin or arrives with a scheme/host of its own.
fn pinned_url(
    base_url: &str,
    path: &str,
    query: &str,
) -> std::result::Result<url::Url, ApiEndError> {
    let base = url::Url::parse(base_url).map_err(|error| ApiEndError {
        code: API_ERROR_DESTINATION_REFUSED.into(),
        message: format!("profile baseUrl: {error}"),
    })?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return Err(ApiEndError {
            code: API_ERROR_DESTINATION_REFUSED.into(),
            message: "profile baseUrl is not an http(s) origin".into(),
        });
    }
    if path.is_empty() || path.starts_with("//") || path.contains("://") {
        return Err(ApiEndError {
            code: API_ERROR_DESTINATION_REFUSED.into(),
            message: "relayed path must name a path under the profile base path".into(),
        });
    }
    // No parent-directory traversal: a `..` segment could resolve back to the
    // origin root even when the joined URL's origin matches.
    if path
        .split('/')
        .any(|segment| segment == ".." || segment.contains('\\'))
    {
        return Err(ApiEndError {
            code: API_ERROR_DESTINATION_REFUSED.into(),
            message: "relayed path must not contain parent-directory traversal".into(),
        });
    }
    let joined = {
        // Two legitimate shapes for a path pinned to a base ending in `/v1`:
        // a root-relative `/v1/messages` (joined at the origin) and a
        // base-relative `messages` (joined under the trailing base path).
        let origin_root = {
            let mut o = base.clone();
            o.set_path("");
            o.set_query(None);
            o
        };
        let relative = path.trim_start_matches('/');
        let root_joined = origin_root.join(relative).map_err(|error| ApiEndError {
            code: API_ERROR_DESTINATION_REFUSED.into(),
            message: format!("joining relayed path: {error}"),
        })?;
        let base_path = base.path().trim_end_matches('/');
        if base_path.is_empty() || root_joined.path().starts_with(&format!("{base_path}/")) {
            root_joined
        } else {
            let mut base_ref = base.clone();
            if !base_ref.path().ends_with('/') {
                base_ref.set_path(&format!("{}/", base.path()));
            }
            base_ref.join(relative).map_err(|error| ApiEndError {
                code: API_ERROR_DESTINATION_REFUSED.into(),
                message: format!("joining relayed path: {error}"),
            })?
        }
    };
    if joined.origin() != base.origin() {
        return Err(ApiEndError {
            code: API_ERROR_DESTINATION_REFUSED.into(),
            message: "relayed path escapes the profile's pinned origin".into(),
        });
    }
    let mut url = joined;
    if !query.is_empty() {
        url.set_query(Some(query.trim_start_matches('?')));
    }
    Ok(url)
}

fn decode_b64(raw: &str) -> RelayResult<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(raw.as_bytes())
        .map_err(|_| "invalid base64 in an api.* chunk".to_string())
}

/// Load the gateway credential for one stream, memory-only.
async fn load_upstream_secret(
    state: &AppState,
    profile: &crate::store::ProviderRecord,
) -> std::result::Result<Option<String>, HubError> {
    if profile.secret_name.as_deref().is_none() {
        return Ok(None);
    }
    let secret = crate::providers::load_secret_for_relay(&state.secrets, profile)?;
    Ok(Some(
        secret
            .expose_str()
            .map_err(|err| HubError::Internal(err.to_string()))?
            .to_string(),
    ))
}

/// Journal the per-stream counters; bodies and headers never appear.
async fn journal_end(
    state: &AppState,
    stream: &Arc<Stream>,
    status: Option<u16>,
    error: Option<&ApiEndError>,
) {
    let (bytes_up, bytes_down, ms) = {
        let st = stream.state.lock().await;
        (
            st.bytes_up,
            st.bytes_down,
            stream.started.elapsed().as_millis() as u64,
        )
    };
    let mut payload = json!({
        "type": "api.stream",
        "streamId": stream.worker_stream,
        "profileId": stream.profile_id,
        "bytesUp": bytes_up,
        "bytesDown": bytes_down,
        "ms": ms,
    });
    if let Some(status) = status {
        payload["status"] = json!(status);
    }
    if let Some(error) = error {
        payload["errorCode"] = json!(error.code);
    }
    if let Ok(Some(record)) = state
        .store
        .append_hub_event(stream.instance_id.clone(), "lifecycle", payload)
        .await
    {
        state.bus.publish(crate::ws::FollowEvent::json(
            stream.instance_id.clone(),
            record.seq,
            record.event.clone(),
        ));
    }
}

/// Project the stream's observed gateway status into the supply-evidence path.
///
/// Uses the real HTTP status, not screen text. Only 429/529 match a signal;
/// everything else is a no-op.
async fn project_supply_status(state: &AppState, stream: &Arc<Stream>) {
    let status = stream.state.lock().await.status;
    project_supply_status_oneshot(state, stream, status).await;
}

async fn project_supply_status_oneshot(
    state: &AppState,
    stream: &Arc<Stream>,
    status: Option<u16>,
) {
    let Some(status) = status else { return };
    if !matches!(status, 429 | 529) {
        return;
    }
    let Ok(Some(profile)) = state.store.get_provider(stream.profile_id.clone()).await else {
        return;
    };
    let mut supply = profile.supply.clone();
    let family = crate::supply::family_of(&profile, profile.default_model.as_deref().unwrap_or(""));
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let signal =
        crate::supply::observe_textual_event(&mut supply.windows, &family, Some(status), "", now);
    if signal.is_some() {
        let _ = state
            .store
            .update_provider_supply(stream.profile_id.clone(), supply)
            .await;
    }
}
