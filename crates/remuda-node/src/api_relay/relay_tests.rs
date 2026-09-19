//! Integration tests for the D-047/D-048 relay, run in-crate so they can reach
//! the listeners, brokers and egress directly. A tiny scripted axum gateway
//! stands in for the real model origin (c-apiroute-fakegw had not landed when
//! this was written).

use super::egress::{CredentialKind, EgressContext, EgressTimeouts};
use super::listener;
use super::policy::RelayBearer;
use super::{ApiRelayState, InboundEvent, LinkBroker};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::StreamExt;
use remuda_protocol::hubnode::{
    METHOD_API_CANCEL, METHOD_API_CHUNK, METHOD_API_END, METHOD_API_HEAD, METHOD_API_OPEN,
};
use remuda_protocol::{
    ApiRouteKind, ApiRouteMode, HostId, HostRelayBind, ProviderDeliveryMode, RequestedApiRoute,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

const INSTANCE: &str = "inst_relay_test";

fn via(route: ApiRouteMode) -> RequestedApiRoute {
    RequestedApiRoute {
        mode: ProviderDeliveryMode::Via,
        via_host_id: Some(HostId::new()),
        route,
    }
}

fn request_with(
    route: Option<RequestedApiRoute>,
    endpoint: Option<&str>,
) -> crate::CreateInstanceRequest {
    let mut value = json!({
        "kind": "claude",
        "driver": "claude-print",
        "model": "haiku",
        "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.invalid/v1" },
    });
    if let Some(route) = route {
        value["apiRoute"] = serde_json::to_value(&route).unwrap();
    }
    if let Some(endpoint) = endpoint {
        value["apiRelayEndpoint"] = json!(endpoint);
    }
    serde_json::from_value(value).expect("request")
}

// ── scripted gateway ───────────────────────────────────────────────────────

#[derive(Default)]
struct GatewaySeen {
    headers: Mutex<Vec<(String, String)>>,
    path: Mutex<Option<String>>,
}

struct Gateway {
    base_url: String,
    seen: Arc<GatewaySeen>,
}

/// Behaviour the scripted origin plays.
#[derive(Clone, Copy)]
enum GatewayMode {
    /// Answer `GET /v1/models` with a small JSON page.
    Models,
    /// Stream `chunks` SSE frames of `size` bytes spaced `gap_ms` apart.
    Sse {
        chunks: usize,
        gap_ms: u64,
        size: usize,
    },
    /// Wait before the first response byte.
    SlowFirstByte { delay_ms: u64 },
}

#[derive(Clone)]
struct GatewayState {
    seen: Arc<GatewaySeen>,
    mode: GatewayMode,
}

async fn models_handler(
    axum::extract::State(state): axum::extract::State<GatewayState>,
) -> impl axum::response::IntoResponse {
    *state.seen.path.lock().unwrap() = Some("/v1/models".into());
    axum::Json(json!({"data": []}))
}

async fn messages_handler(
    axum::extract::State(state): axum::extract::State<GatewayState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (parts, _) = request.into_parts();
    *state.seen.path.lock().unwrap() = Some(parts.uri.to_string());
    for name in [
        "authorization",
        "x-api-key",
        "cookie",
        "x-custom-denied",
        "x-profile-header",
    ] {
        if let Some(value) = parts.headers.get(name) {
            state
                .seen
                .headers
                .lock()
                .unwrap()
                .push((name.to_owned(), value.to_str().unwrap_or("").to_owned()));
        }
    }
    match state.mode {
        GatewayMode::Models => unreachable!(),
        GatewayMode::Sse {
            chunks,
            gap_ms,
            size,
        } => sse_response(chunks, gap_ms, size),
        GatewayMode::SlowFirstByte { delay_ms } => {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            axum::Json(json!({"ok": true})).into_response()
        }
    }
}

async fn spawn_gateway(mode: GatewayMode) -> Gateway {
    use axum::Router;
    use axum::routing::get;

    let seen = Arc::new(GatewaySeen::default());
    let state = GatewayState {
        seen: Arc::clone(&seen),
        mode,
    };
    let app = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/messages", axum::routing::post(messages_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Gateway {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        seen,
    }
}

fn sse_response(chunks: usize, gap_ms: u64, size: usize) -> axum::response::Response {
    let stream = futures::stream::iter(0..chunks).then(move |index| async move {
        tokio::time::sleep(Duration::from_millis(if index == 0 { 0 } else { gap_ms })).await;
        let mut frame = format!("event: chunk{index}\n").into_bytes();
        frame.extend(std::iter::repeat_n(b'a', size));
        frame.extend_from_slice(b"\n\n");
        Ok::<_, std::io::Error>(Bytes::from(frame))
    });
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("set-cookie", "must-not-cross=1")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

// ── wiring ─────────────────────────────────────────────────────────────────

/// Connect two nodes' brokers the way the Hub would: every outbound frame on
/// one is fed inbound to the other. `log` records the frame methods.
fn wire_like_hub(
    state_w: &Arc<ApiRelayState>,
    state_h: &Arc<ApiRelayState>,
    log: Arc<Mutex<Vec<String>>>,
) -> (Arc<LinkBroker>, Arc<LinkBroker>, JoinHandle<()>) {
    let (broker_w, mut rx_w) = state_w.attach_link();
    let (broker_h, mut rx_h) = state_h.attach_link();
    let (w, h, log) = (
        Arc::clone(&broker_w),
        Arc::clone(&broker_h),
        Arc::clone(&log),
    );
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = rx_w.recv() => {
                    let Some(frame) = frame else { break };
                    if let Some(method) = frame.get("method").and_then(Value::as_str) {
                        log.lock().unwrap().push(method.to_owned());
                    }
                    h.handle_frame(&frame).await;
                }
                frame = rx_h.recv() => {
                    let Some(frame) = frame else { break };
                    if let Some(method) = frame.get("method").and_then(Value::as_str) {
                        log.lock().unwrap().push(method.to_owned());
                    }
                    w.handle_frame(&frame).await;
                }
            }
        }
    });
    (broker_w, broker_h, task)
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

fn gateway_context(
    gateway: &Gateway,
    tuning: Option<(usize, EgressTimeouts)>,
) -> Arc<EgressContext> {
    let mut context = EgressContext::new(
        INSTANCE,
        &format!("{}/v1", gateway.base_url),
        CredentialKind::GatewayBearer("gw-secret-token".into()),
    )
    .unwrap()
    .with_profile_headers([("x-profile-header", "profile-value")]);
    if let Some((coalesce, timeouts)) = tuning {
        context = context.with_test_tuning(coalesce, timeouts);
    }
    context
}

// ── listener authorization ─────────────────────────────────────────────────

#[tokio::test]
async fn worker_listener_auth_matrix() {
    let state = ApiRelayState::new();
    let provision = listener::provision_worker(
        &state,
        INSTANCE,
        "http://gateway.invalid/v1",
        &via(ApiRouteMode::HubRelay),
        None,
    )
    .await
    .expect("provision");
    let url = provision.listener.base_url();
    assert_eq!(provision.kind, ApiRouteKind::HubRelay);
    // lsof-free loopback assertion: the bound address itself is loopback.
    assert!(provision.listener.local_addr().ip().is_loopback());
    let bearer = provision.listener.bearer_token();
    assert_eq!(BASE64.decode(bearer.as_bytes()).unwrap().len(), 32);
    let client = http();

    let status = client
        .get(format!("{url}/models"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 403, "no bearer");

    let status = client
        .get(format!("{url}/models"))
        .bearer_auth("wrong-token")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 403, "wrong bearer");

    let status = client
        .request(reqwest::Method::PUT, format!("{url}/models"))
        .bearer_auth(&bearer)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 404, "method outside GET or POST");

    let outside = url
        .rsplit_once('/')
        .map_or(url.as_str(), |(origin, _)| origin);
    let status = client
        .get(format!("{outside}/elsewhere"))
        .bearer_auth(&bearer)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 404, "path outside the profile base path");

    // Authenticated but no Hub link: a 503 Anthropic-shaped body, not a hang.
    let response = client
        .get(format!("{url}/models"))
        .bearer_auth(&bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert!(body.contains("via-host-offline"), "{body}");

    // Exit revokes the bearer and shuts the listener. A new request then
    // either fails at the socket or answers 403, depending on how far the
    // graceful shutdown got; both mean "the instance is gone".
    state.revoke_instance(INSTANCE);
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Ok(response) = client
        .get(format!("{url}/models"))
        .bearer_auth(&bearer)
        .send()
        .await
    {
        assert_eq!(
            response.status(),
            403,
            "revoked bearer must not authenticate"
        );
    } // an Err means the listener already closed — equally refused
}

// ── hub-relay end to end ───────────────────────────────────────────────────

#[tokio::test]
async fn hub_relay_forwards_and_streams_sse_with_credential_swap() {
    // 8 KiB per SSE frame, ~30 ms apart: coalescing must merge them and the
    // credit loop must keep the transfer moving.
    let gateway = spawn_gateway(GatewayMode::Sse {
        chunks: 6,
        gap_ms: 30,
        size: 8 * 1024,
    })
    .await;
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    state_h.set_egress_context(gateway_context(&gateway, None));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::clone(&log));

    let bearer = RelayBearer::mint();
    let provision = listener::provision_worker_with_bearer(
        &state_w,
        INSTANCE,
        &format!("{}/v1", gateway.base_url),
        &via(ApiRouteMode::HubRelay),
        None,
        bearer,
    )
    .await
    .unwrap();
    let url = provision.listener.base_url();
    let bearer = provision.listener.bearer_token();

    let response = http()
        .post(format!("{url}/messages"))
        .bearer_auth(&bearer)
        .header("cookie", "session=stolen")
        .header("x-custom-denied", "x")
        .json(&json!({"model": "haiku", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    // set-cookie is stripped by the response allowlist.
    assert!(response.headers().get("set-cookie").is_none());
    let body = response.text().await.unwrap();
    assert!(body.contains("event: chunk0"));
    assert!(body.contains("event: chunk5"));
    assert_eq!(
        body.len(),
        6 * (b"event: chunkN\n".len() + 8 * 1024 + b"\n\n".len())
    );

    // The gateway saw the profile credential — never the relay bearer or a
    // client-supplied cookie/header.
    let seen = gateway.seen.headers.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer gw-secret-token"),
        "{seen:?}"
    );
    assert!(!seen.iter().any(|(_, value)| value.contains(&bearer)));
    assert!(!seen.iter().any(|(name, _)| name == "cookie"));
    assert!(!seen.iter().any(|(name, _)| name == "x-custom-denied"));
    assert!(
        seen.iter()
            .any(|(name, value)| name == "x-profile-header" && value == "profile-value"),
        "profile headers are installed on the egress request"
    );
    drop(seen);

    let frames = log.lock().unwrap();
    assert!(frames.iter().any(|m| m == METHOD_API_OPEN));
    assert!(frames.iter().any(|m| m == METHOD_API_HEAD));
    assert!(frames.iter().any(|m| m == METHOD_API_END));
    // 48 KiB at a 16 KiB coalescing threshold plus the final flush must be
    // several chunks, and every one of them rode a credit back from W.
    let chunks = frames.iter().filter(|m| **m == METHOD_API_CHUNK).count();
    assert!((2..=8).contains(&chunks), "chunks={chunks}");
    let credits = frames
        .iter()
        .filter(|m| **m == remuda_protocol::hubnode::METHOD_API_CREDIT)
        .count();
    assert!(credits >= chunks, "every chunk is drained and credited");
}

#[tokio::test]
async fn client_disconnect_sends_api_cancel() {
    let gateway = spawn_gateway(GatewayMode::Sse {
        chunks: 40,
        gap_ms: 25,
        size: 1024,
    })
    .await;
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    state_h.set_egress_context(gateway_context(&gateway, None));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::clone(&log));

    let provision = listener::provision_worker(
        &state_w,
        INSTANCE,
        &format!("{}/v1", gateway.base_url),
        &via(ApiRouteMode::HubRelay),
        None,
    )
    .await
    .unwrap();
    let url = provision.listener.base_url();
    let bearer = provision.listener.bearer_token();

    let response = http()
        .post(format!("{url}/messages"))
        .bearer_auth(&bearer)
        .json(&json!({"stream": true}))
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let _first = stream.next().await;
    drop(stream); // client goes away mid-response

    // The drive task must have sent api.cancel toward H.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if log.lock().unwrap().iter().any(|m| m == METHOD_API_CANCEL) {
                return;
            }
        }
    })
    .await
    .expect("api.cancel after a client disconnect");
}

#[tokio::test]
async fn slow_gateway_first_byte_is_a_504_api_error() {
    let gateway = spawn_gateway(GatewayMode::SlowFirstByte { delay_ms: 2_000 }).await;
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let tuned = gateway_context(
        &gateway,
        Some((
            16 * 1024,
            EgressTimeouts {
                ttft: Duration::from_millis(300),
                idle: Duration::from_secs(2),
                hard: Duration::from_secs(5),
            },
        )),
    );
    state_h.set_egress_context(tuned);
    let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::new(Mutex::new(Vec::new())));

    let provision = listener::provision_worker(
        &state_w,
        INSTANCE,
        &format!("{}/v1", gateway.base_url),
        &via(ApiRouteMode::HubRelay),
        None,
    )
    .await
    .unwrap();
    let response = http()
        .post(format!("{}/messages", provision.listener.base_url()))
        .bearer_auth(provision.listener.bearer_token())
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 504);
    let body = response.text().await.unwrap();
    assert!(body.contains("upstream-timeout"), "{body}");
}

// ── egress policy ──────────────────────────────────────────────────────────

#[tokio::test]
async fn egress_refuses_path_escape_and_missing_context() {
    let state_h = ApiRelayState::new();
    let (broker, mut rx) = state_h.attach_link();

    // Context pinned to one origin; a frame that walks off the base path ends
    // the stream with destination-refused, never touching a network.
    let context = EgressContext::new(
        "pinned",
        "http://127.0.0.1:1/v1",
        CredentialKind::GatewayBearer("secret".into()),
    )
    .unwrap();
    state_h.set_egress_context(context);

    broker
        .handle_frame(&open_frame("pinned", "/v1/../../etc/passwd"))
        .await;
    let end = recv_method(&mut rx, METHOD_API_END).await;
    assert_eq!(end["params"]["error"]["code"], "destination-refused");

    // An instance with no pushed context is refused identically: an
    // unconfigured Node cannot be made an open proxy.
    broker
        .handle_frame(&open_frame("nobody-home", "/messages"))
        .await;
    let end = recv_method(&mut rx, METHOD_API_END).await;
    assert_eq!(end["params"]["error"]["code"], "destination-refused");
}

fn open_frame(instance_id: &str, path: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": METHOD_API_OPEN,
        "params": {
            "instanceId": instance_id,
            "streamId": format!("strm_{instance_id}_{}", uuid::Uuid::new_v4().simple()),
            "method": "GET",
            "path": path,
            "query": "",
            "headers": [],
            "bodyChunked": false,
            "deadlineMs": 30000
        }
    })
}

async fn recv_method(rx: &mut tokio::sync::mpsc::Receiver<Value>, method: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(frame) = rx.recv().await {
            if frame.get("method").and_then(Value::as_str) == Some(method) {
                return frame;
            }
        }
        panic!("no {method} frame");
    })
    .await
    .unwrap()
}

// ── direct-net ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn direct_net_probe_and_request_end_to_end() {
    let gateway = spawn_gateway(GatewayMode::Models).await;
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let bearer = RelayBearer::mint();

    // H's proxy listener, explicitly bound to a loopback address (the only
    // kind the bind policy allows without an operator setting).
    let proxy = listener::start_proxy(
        &state_h,
        INSTANCE,
        &HostRelayBind {
            addr: format!("127.0.0.1:{}", 0),
            allow_from: Vec::new(),
        },
        gateway_context(&gateway, None),
        bearer,
    )
    .await
    .unwrap();
    let endpoint = format!("http://127.0.0.1:{}", proxy.local_addr().port());

    // auto + a probing endpoint promotes the route to direct-net.
    let provision = listener::provision_worker_with_bearer(
        &state_w,
        INSTANCE,
        &format!("{}/v1", gateway.base_url),
        &via(ApiRouteMode::Auto),
        Some(&endpoint),
        RelayBearer::from_encoded(proxy.bearer_token()),
    )
    .await
    .unwrap();
    assert_eq!(provision.kind, ApiRouteKind::DirectNet);

    let response = http()
        .get(format!("{}/models", provision.listener.base_url()))
        .bearer_auth(provision.listener.bearer_token())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("data"));
}

#[tokio::test]
async fn direct_net_refusals() {
    let state = ApiRelayState::new();

    // direct-net with no configured endpoint refuses with the stable code.
    let error = listener::provision_worker(
        &state,
        INSTANCE,
        "http://gateway.invalid/v1",
        &via(ApiRouteMode::DirectNet),
        None,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("api-via-unreachable"), "{error}");

    // An endpoint that refuses the probe fails direct-net.
    let error = listener::provision_worker(
        &state,
        INSTANCE,
        "http://gateway.invalid/v1",
        &via(ApiRouteMode::DirectNet),
        Some("http://127.0.0.1:1"),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("api-via-unreachable"), "{error}");

    // auto with an unreachable endpoint silently takes the path that always
    // works — but the observed route says hub-relay truthfully (D-035).
    let provision = listener::provision_worker(
        &state,
        INSTANCE,
        "http://gateway.invalid/v1",
        &via(ApiRouteMode::Auto),
        Some("http://127.0.0.1:1"),
    )
    .await
    .unwrap();
    assert_eq!(provision.kind, ApiRouteKind::HubRelay);

    // A relayBind on a public or any-address is refused before binding.
    let context = EgressContext::new(
        INSTANCE,
        "http://gateway.invalid/v1",
        CredentialKind::ApiKey("k".into()),
    )
    .unwrap();
    assert!(
        listener::start_proxy(
            &state,
            INSTANCE,
            &HostRelayBind {
                addr: "0.0.0.0:8443".into(),
                allow_from: Vec::new(),
            },
            Arc::clone(&context),
            RelayBearer::mint(),
        )
        .await
        .is_err()
    );
    assert!(
        listener::start_proxy(
            &state,
            INSTANCE,
            &HostRelayBind {
                addr: "8.8.8.8:8443".into(),
                allow_from: Vec::new(),
            },
            context,
            RelayBearer::mint(),
        )
        .await
        .is_err()
    );
}

// ── broker table ───────────────────────────────────────────────────────────

#[tokio::test]
async fn broker_enforces_stream_caps_and_demuxes() {
    let state = ApiRelayState::new();
    let (broker, mut rx) = state.attach_link();

    let mut worker_streams = Vec::new();
    // Two per instance …
    for instance in ["i1", "i2", "i3", "i4"] {
        for _ in 0..2 {
            let opened = broker.open_worker_stream(instance).unwrap();
            worker_streams.push(opened);
        }
        // …and a third for the same instance is refused.
        assert!(broker.open_worker_stream(instance).is_err());
    }
    // Eight live streams on the link is the cap.
    assert!(broker.open_worker_stream("i5").is_err());

    // Ending one stream frees exactly its slots.
    let (id, _outbox, mut events) = worker_streams.pop().unwrap();
    broker
        .handle_frame(&json!({
            "jsonrpc": "2.0",
            "method": remuda_protocol::hubnode::METHOD_API_END,
            "params": {"streamId": id, "bytesUp": 0, "bytesDown": 0, "ms": 1}
        }))
        .await;
    assert!(matches!(events.recv().await, Some(InboundEvent::End(_))));
    assert!(broker.open_worker_stream("i5").is_ok());

    // Unrelated frames are not consumed; api frames for unknown ids are.
    assert!(
        !broker
            .handle_frame(&json!({"jsonrpc":"2.0","method":"tty.frame","params":{}}))
            .await
    );
    broker
        .handle_frame(&json!({
            "jsonrpc": "2.0",
            "method": remuda_protocol::hubnode::METHOD_API_CHUNK,
            "params": {"streamId": "strm_dead", "seq": 0, "dataBase64": "", "last": true}
        }))
        .await;

    // Link loss ends every remaining stream with the stable code.
    broker.fail_all(
        remuda_protocol::hubnode::API_ERROR_HUB_LINK_LOST,
        "test link loss",
    );
    for (_, _outbox, mut events) in worker_streams {
        let event = events.recv().await.expect("end event");
        match event {
            InboundEvent::End(end) => {
                assert_eq!(end.error.unwrap().code, "hub-link-lost");
            }
            other => panic!("expected end, got {other:?}"),
        }
    }
    while rx.try_recv().is_ok() {}
}

#[tokio::test]
async fn producer_waits_for_credits_then_moves() {
    let state = ApiRelayState::new();
    let (broker, _rx) = state.attach_link();
    let (id, outbox, mut events) = broker.open_worker_stream("inst").unwrap();

    // The owner grants its own window from inbound api.credit, exactly as
    // drive_inband does when it drains a chunk.
    let grantor = outbox.clone();
    tokio::spawn(async move {
        while let Some(InboundEvent::Credit(credit)) = events.recv().await {
            grantor.grant(credit.chunks);
        }
    });

    // The initial window is four unacknowledged chunks.
    for seq in 0..4u32 {
        outbox
            .send_gated(
                remuda_protocol::hubnode::METHOD_API_BODY,
                &remuda_protocol::hubnode::ApiBodyParams {
                    stream_id: id.clone(),
                    seq,
                    data_base64: BASE64.encode(b"x"),
                    last: false,
                },
            )
            .await
            .unwrap();
    }
    let producer = tokio::spawn({
        let outbox = outbox.clone();
        let id = id.clone();
        async move {
            outbox
                .send_gated(
                    remuda_protocol::hubnode::METHOD_API_BODY,
                    &remuda_protocol::hubnode::ApiBodyParams {
                        stream_id: id,
                        seq: 4,
                        data_base64: BASE64.encode(b"x"),
                        last: false,
                    },
                )
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!producer.is_finished(), "producer must be credit-gated");
    broker
        .handle_frame(&json!({
            "jsonrpc": "2.0",
            "method": remuda_protocol::hubnode::METHOD_API_CREDIT,
            "params": {"streamId": id, "chunks": 1}
        }))
        .await;
    producer
        .await
        .expect("producer task")
        .expect("gated send completes once a credit arrives");
}

// ── provision validation ───────────────────────────────────────────────────

#[tokio::test]
async fn via_without_host_is_refused_not_projected_direct() {
    let state = ApiRelayState::new();
    let mut request = request_with(None, None);
    request.api_route = Some(RequestedApiRoute {
        mode: ProviderDeliveryMode::Via,
        via_host_id: None,
        route: ApiRouteMode::HubRelay,
    });
    let error = listener::provision_for_request(&state, INSTANCE, &request)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("viaHostId"), "{error}");
}

#[tokio::test]
async fn direct_request_provisions_nothing() {
    let state = ApiRelayState::new();
    let request = request_with(None, None);
    assert!(
        listener::provision_for_request(&state, INSTANCE, &request)
            .await
            .unwrap()
            .is_none()
    );

    let mut request = request_with(None, None);
    request.api_route = Some(RequestedApiRoute {
        mode: ProviderDeliveryMode::Direct,
        via_host_id: None,
        route: ApiRouteMode::Auto,
    });
    assert!(
        listener::provision_for_request(&state, INSTANCE, &request)
            .await
            .unwrap()
            .is_none()
    );
}
