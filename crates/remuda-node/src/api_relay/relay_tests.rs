//! Integration tests for the D-047/D-048 relay, run in-crate so they can reach
//! the listeners, brokers and egress directly. The model origin is the shared
//! offline [`remuda_testing::FakeGateway`]: no test here ever reaches a real model
//! or holds a real credential, and every fixture token carries the `fake-`
//! segment required by `scripts/ci/secret-scan.sh`.

use super::egress::{CredentialKind, EgressContext, EgressTimeouts};
use super::listener;
use super::policy::RelayBearer;
use super::{ApiRelayState, InboundEvent, LinkBroker};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use remuda_protocol::hubnode::{
    ApiOpenParams, METHOD_API_CANCEL, METHOD_API_CHUNK, METHOD_API_CREDIT, METHOD_API_END,
    METHOD_API_HEAD, METHOD_API_OPEN,
};
use remuda_protocol::{
    ApiRouteKind, ApiRouteMode, HostId, HostRelayBind, ProviderDeliveryMode, RequestedApiRoute,
};
use remuda_testing::{FakeGateway, SSE_CONTENT_TYPE, Script};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

const INSTANCE: &str = "inst_relay_test";
/// Fixture profile credential. The delimited `fake-` segment is mandatory for
/// test credentials; the gateway compares the token and never exposes it.
const GW_TOKEN: &str = "sk-fake-relay-profile-credential";

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

// ── wiring ─────────────────────────────────────────────────────────────────

/// Direction a logged frame crossed the Hub-shaped test link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameDir {
    /// Worker → Hub → proxy (e.g. api.open, api.body, api.cancel from W).
    WtoH,
    /// Proxy → Hub → worker (api.head, api.chunk, api.end, response credits).
    HtoW,
}

/// One logged frame: its direction, method, and for `api.chunk` the ordering
/// seq. Direction matters: W credits H for request bytes and H credits W for
/// response bytes, so a one-list count could never prove the right side sent
/// them.
#[derive(Debug, Clone)]
struct FrameLog {
    dir: FrameDir,
    method: String,
    chunk_seq: Option<u32>,
}

fn record_frame(log: &Mutex<Vec<FrameLog>>, dir: FrameDir, frame: &Value) {
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        return;
    };
    let chunk_seq = (method == METHOD_API_CHUNK)
        .then(|| {
            frame
                .pointer("/params/seq")
                .and_then(Value::as_u64)
                .map(|seq| seq as u32)
        })
        .flatten();
    log.lock().unwrap().push(FrameLog {
        dir,
        method: method.to_owned(),
        chunk_seq,
    });
}

/// Connect two nodes' brokers the way the Hub would: every outbound frame on
/// one is fed inbound to the other. `log` records each frame's direction.
fn wire_like_hub(
    state_w: &Arc<ApiRelayState>,
    state_h: &Arc<ApiRelayState>,
    log: Arc<Mutex<Vec<FrameLog>>>,
) -> (Arc<LinkBroker>, Arc<LinkBroker>, JoinHandle<()>) {
    let (broker_w, mut rx_w) = state_w.attach_link();
    let (broker_h, mut rx_h) = state_h.attach_link();
    let (w, h) = (Arc::clone(&broker_w), Arc::clone(&broker_h));
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = rx_w.recv() => {
                    let Some(frame) = frame else { break };
                    record_frame(&log, FrameDir::WtoH, &frame);
                    h.handle_frame(&frame).await;
                }
                frame = rx_h.recv() => {
                    let Some(frame) = frame else { break };
                    record_frame(&log, FrameDir::HtoW, &frame);
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
    gateway: &FakeGateway,
    tuning: Option<(usize, EgressTimeouts)>,
) -> Arc<EgressContext> {
    gateway.expect_credential(GW_TOKEN);
    gateway.set_response_header("set-cookie", "must-not-cross=1");
    let mut context = EgressContext::new(
        INSTANCE,
        &gateway.base_url_v1(),
        CredentialKind::GatewayBearer(GW_TOKEN.into()),
    )
    .unwrap()
    .with_profile_headers([("x-profile-header", "profile-value")]);
    if let Some((coalesce, timeouts)) = tuning {
        context = context.with_test_tuning(coalesce, timeouts);
    }
    context
}

/// Provision W, install a credentialed FakeGateway context on H, and wire the
/// two brokers Hub-style.
async fn hub_relay_fixture(
    scripts: Vec<Script>,
) -> (
    Arc<ApiRelayState>,
    Arc<ApiRelayState>,
    FakeGateway,
    Arc<Mutex<Vec<FrameLog>>>,
    String,
    String,
) {
    let gateway = FakeGateway::start_with(scripts)
        .await
        .expect("fake gateway");
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    state_h.set_egress_context(gateway_context(&gateway, None));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::clone(&log));

    let bearer = RelayBearer::mint();
    let provision = listener::provision_worker_with_bearer(
        &state_w,
        INSTANCE,
        &gateway.base_url_v1(),
        &via(ApiRouteMode::HubRelay),
        None,
        bearer,
    )
    .await
    .unwrap();
    let url = provision.listener.base_url();
    let bearer = provision.listener.bearer_token();
    (state_w, state_h, gateway, log, url, bearer)
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

    // Exit revokes the bearer and shuts the listener.
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

// ── hub-relay streaming ──────────────────────────────────────────────────────

#[tokio::test]
async fn hub_relay_streams_sse_with_credential_swap_in_order() {
    // The fixture's deterministic SSE script: message_start,
    // content_block_start, ordered content_block_delta tokens,
    // content_block_stop, message_delta, message_stop — the relay must
    // preserve the Anthropic event sequence, not byte-sort it.
    let (_sw, _sh, gateway, log, url, bearer) =
        hub_relay_fixture(vec![Script::default_messages()]).await;

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
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some(SSE_CONTENT_TYPE)
    );
    // The fixture advertises a set-cookie the relay response allowlist drops.
    assert!(response.headers().get("set-cookie").is_none());
    let body = response.text().await.unwrap();

    // SSE order by sequence: the fixture's deterministic event order must
    // survive coalescing and reassembly.
    let ordered = [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ];
    let mut cursor = 0usize;
    for name in ordered {
        let at = body[cursor..]
            .find(name)
            .unwrap_or_else(|| panic!("missing {name}"));
        cursor += at;
    }
    // content_block_delta appears multiple times and strictly after the
    // block start and before the block stop.
    assert!(body.matches("content_block_delta").count() > 1);

    // Credential swap, proven by value-free verdicts: the gateway saw exactly
    // the fixture credential, never a mismatch (the relay bearer would have
    // produced one), and the cookie/denied header never reached it.
    gateway
        .assert_presented_expected_credential()
        .expect("egress installed the profile credential");
    assert!(
        !gateway.saw_credential_mismatch(),
        "relay bearer never reached the gateway"
    );
    let recorded = gateway.requests_to("/v1/messages");
    let message = recorded.first().expect("gateway saw /v1/messages");
    assert!(
        message.has_header("x-profile-header"),
        "profile header installed on egress"
    );
    assert!(!message.has_header("cookie"), "client cookie dropped");
    assert!(
        !message.has_header("x-custom-denied"),
        "unlisted request header dropped"
    );

    let frames = log.lock().unwrap();
    assert!(frames.iter().any(|f| f.method == METHOD_API_OPEN));
    assert!(frames.iter().any(|f| f.method == METHOD_API_HEAD));
    assert!(frames.iter().any(|f| f.method == METHOD_API_END));
    // The default script is a few hundred bytes under the 16 KiB threshold;
    // at least one body chunk must still flow.
    let chunks = frames
        .iter()
        .filter(|f| f.method == METHOD_API_CHUNK)
        .count();
    assert!(chunks >= 1, "a coalesced SSE body rode api.chunk");
    // Credits the W side returns for drained *response* chunks travel H←W;
    // count that direction explicitly.
    let credits = frames
        .iter()
        .filter(|f| f.dir == FrameDir::WtoH && f.method == METHOD_API_CREDIT)
        .count();
    assert!(
        credits >= chunks,
        "every response chunk is drained and credited"
    );

    // The seq on api.chunk is the Hub's ordering key: strictly increasing
    // from zero with no repeats — the tail flush and the zero-byte last chunk
    // included (a regression reused the tail's seq for the terminator).
    let seqs: Vec<u32> = frames.iter().filter_map(|f| f.chunk_seq).collect();
    assert_eq!(
        *seqs.first().unwrap_or(&u32::MAX),
        0,
        "seqs start at 0: {seqs:?}"
    );
    assert!(
        seqs.windows(2).all(|pair| pair[0] + 1 == pair[1]),
        "api.chunk seqs must be contiguous and strictly increasing: {seqs:?}"
    );
    assert_eq!(seqs.len(), chunks, "every chunk carries one seq");
}

#[tokio::test]
async fn client_disconnect_sends_api_cancel() {
    // A script long enough to still be streaming when the client drops.
    let (_sw, _sh, _gateway, log, url, bearer) =
        hub_relay_fixture(vec![Script::messages("a".repeat(512 * 1024))]).await;

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

    // The drive task must have sent api.cancel toward H (W→H direction).
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let sent = log
                .lock()
                .unwrap()
                .iter()
                .any(|f| f.dir == FrameDir::WtoH && f.method == METHOD_API_CANCEL);
            if sent {
                return;
            }
        }
    })
    .await
    .expect("api.cancel after a client disconnect");
}

#[tokio::test]
async fn slow_first_byte_is_mapped_to_504_anthropic_body() {
    let gateway = FakeGateway::start_with(vec![Script::slow_first_byte(Duration::from_secs(2))])
        .await
        .unwrap();
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
        &gateway.base_url_v1(),
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
    // The pinned gateway origin never appears in the body handed to W.
    assert!(
        !body.contains(&gateway.base_url_v1()),
        "origin must not reach W"
    );
}

// ── origin confinement on the error path ───────────────────────────────────

#[tokio::test]
async fn gateway_failure_body_never_carries_the_pinned_origin_to_w() {
    // The happy path is covered by the credential-swap test; this exercises
    // the reqwest *error* path end to end: a pinned origin that refuses the
    // connection produces an Anthropic-shaped body on W containing only the
    // stable code and fixed text — never the origin's host or port, which
    // reqwest's Display would otherwise embed.
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::new(Mutex::new(Vec::new())));
    let dead_origin = "http://127.0.0.1:9/v1".to_owned();
    let context = EgressContext::new(
        INSTANCE,
        &dead_origin,
        CredentialKind::GatewayBearer(GW_TOKEN.into()),
    )
    .unwrap();
    state_h.set_egress_context(context);
    let provision = listener::provision_worker(
        &state_w,
        INSTANCE,
        &dead_origin,
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
    assert!(
        !body.contains("127.0.0.1") && !body.contains(":9"),
        "the pinned origin must never be rendered toward W: {body}"
    );
    assert!(
        serde_json::from_str::<Value>(&body).is_ok(),
        "the fixed error text still produces valid JSON"
    );
    state_w.revoke_instance(INSTANCE);
}

// ── status passthrough: 429 / 529 with retry-after ────────────────────────

#[tokio::test]
async fn scripted_429_and_529_pass_through_with_retry_after() {
    for (code, error_type) in [(429u16, "rate_limit_error"), (529u16, "overloaded_error")] {
        let gateway = FakeGateway::start_with(vec![Script::status(code)])
            .await
            .unwrap();
        gateway.expect_credential(GW_TOKEN);
        let state_w = ApiRelayState::new();
        let state_h = ApiRelayState::new();
        state_h.set_egress_context(gateway_context(&gateway, None));
        let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::new(Mutex::new(Vec::new())));
        let provision = listener::provision_worker(
            &state_w,
            INSTANCE,
            &gateway.base_url_v1(),
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
        assert_eq!(response.status().as_u16(), code, "{error_type}");
        assert!(
            response.headers().contains_key("retry-after"),
            "retry-after survives the response allowlist for {code}"
        );
        let body = response.text().await.unwrap();
        assert!(body.contains(error_type), "body carries the scripted error");
        // The fixture's status body never carries the gateway origin.
        assert!(!body.contains("127.0.0.1"), "origin must not reach W");
        state_w.revoke_instance(INSTANCE);
    }
}

// ── chunked request body ─────────────────────────────────────────────────────

#[tokio::test]
async fn request_body_larger_than_one_chunk_streams_to_gateway() {
    // Roughly one MiB: well beyond the initial four-chunk credit window, so a
    // pass proves H's per-drained-chunk api.credit replies keep the whole
    // upload moving (not just the first window). The fixture records only a
    // byte count, so end-to-end delivery of every byte is the assertion.
    let gateway = FakeGateway::start_with(vec![Script::messages_json("ok")])
        .await
        .unwrap();
    gateway.expect_credential(GW_TOKEN);
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    state_h.set_egress_context(gateway_context(&gateway, None));
    let (_bw, _bh, _hub) = wire_like_hub(&state_w, &state_h, Arc::clone(&log));
    let provision = listener::provision_worker(
        &state_w,
        INSTANCE,
        &gateway.base_url_v1(),
        &via(ApiRouteMode::HubRelay),
        None,
    )
    .await
    .unwrap();

    // > one wire chunk: axum coalesces small reads but never reads a body this
    // large inline, so this must take the api.body upload leg.
    let body = json!({
        "model": "haiku",
        "padding": "z".repeat(1024 * 1024),
    });
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let body_len = body_bytes.len();
    assert!(
        body_len > 4 * 64 * 1024,
        "the fixture body must exceed the initial credit window"
    );

    let response = http()
        .post(format!("{}/messages", provision.listener.base_url()))
        .bearer_auth(provision.listener.bearer_token())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // The gateway received the whole body, in order, and the W-side log shows
    // many body frames plus the zero-byte last terminator.
    let recorded = gateway.requests_to("/v1/messages");
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].body_bytes, body_len,
        "the gateway got every request byte"
    );
    let frames = log.lock().unwrap();
    // Request bytes ride W→H only.
    let body_frames = frames
        .iter()
        .filter(|f| {
            f.dir == FrameDir::WtoH && f.method == remuda_protocol::hubnode::METHOD_API_BODY
        })
        .count();
    assert!(
        body_frames > 4,
        "a ~1 MiB body rode more than the initial window of api.body frames"
    );
    assert!(
        !frames
            .iter()
            .any(|f| f.dir == FrameDir::HtoW
                && f.method == remuda_protocol::hubnode::METHOD_API_BODY),
        "api.body never travels H→W"
    );
    // The credits that pace an *upload* travel H→W, one per drained chunk —
    // asserted by direction so response credits could not inflate the count.
    let upload_credits = frames
        .iter()
        .filter(|f| {
            f.dir == FrameDir::HtoW && f.method == remuda_protocol::hubnode::METHOD_API_CREDIT
        })
        .count();
    assert!(
        upload_credits >= body_frames - 1,
        "H credits each drained request-body chunk (H→W): {upload_credits} vs {body_frames}"
    );
}

// ── egress policy ──────────────────────────────────────────────────────────

#[tokio::test]
async fn egress_refuses_path_escape_and_missing_context() {
    // A live gateway watches for traffic the refused frames must never
    // generate: the refusal happens before any socket is opened, so zero
    // requests and zero credential-bearing material reach it.
    let gateway = FakeGateway::start_with(vec![Script::default_messages()])
        .await
        .unwrap();
    gateway.expect_credential(GW_TOKEN);
    let state_h = ApiRelayState::new();
    let (broker, mut rx) = state_h.attach_link();

    // Context pinned to one origin; a frame that walks off the base path ends
    // the stream with destination-refused, never touching a network.
    let context = EgressContext::new(
        "pinned",
        &gateway.base_url_v1(),
        CredentialKind::GatewayBearer(GW_TOKEN.into()),
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

    assert_eq!(
        gateway.request_count(),
        0,
        "a refusal never reaches the gateway"
    );
    gateway
        .assert_no_credentials()
        .expect("no credential rides a refused open");
}

#[tokio::test]
async fn hello_limits_lower_chunk_size_and_stream_cap() {
    let state = ApiRelayState::new();
    // Protocol defaults before a hello.
    assert_eq!(
        state.api_chunk_bytes(),
        remuda_protocol::default_api_chunk_bytes() as usize
    );
    assert_eq!(
        state.max_link_streams(),
        remuda_protocol::default_max_api_streams() as usize
    );
    state.apply_hello_limits(&json!({
        "result": {"limits": {"apiChunkBytes": 4096u32, "maxApiStreams": 3u32}}
    }));
    assert_eq!(state.api_chunk_bytes(), 4096);
    assert_eq!(state.max_link_streams(), 3);

    let (broker, _rx) = state.attach_link();
    // Two per instance is still the smaller cap; spread three opens across
    // instances so the fourth hits the Hub-lowered *link* cap.
    // Held for their slots' lifetime; the assertion below is the point.
    let _opened = [
        broker.open_worker_stream("i-a").unwrap().0,
        broker.open_worker_stream("i-a").unwrap().0,
        broker.open_worker_stream("i-b").unwrap().0,
    ];
    assert!(
        broker.open_worker_stream("i-c").is_err(),
        "the Hub-lowered link cap is enforced"
    );
    // Garbage / zero values leave the negotiated limits in place.
    state.apply_hello_limits(&json!({"result": {"limits": {"apiChunkBytes": 0}}}));
    assert_eq!(state.api_chunk_bytes(), 4096);
    broker.fail_all("x", "x");
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
    let gateway = FakeGateway::start_with(vec![Script::default_messages()])
        .await
        .unwrap();
    gateway.expect_credential(GW_TOKEN);
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
        &gateway.base_url_v1(),
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
async fn direct_net_unreachable_gateway_is_a_504_upstream_timeout() {
    // The direct leg must classify a failed gateway connect the same way the
    // framed leg does: 504 + upstream-timeout (the protocol defines that code
    // as "gateway unreachable or timeout ladder breached"), never
    // via-host-offline.
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let bearer = RelayBearer::mint();

    // H's proxy listener answers its own probe (so the worker accepts
    // direct-net) but its pinned gateway origin refuses connections.
    let dead_origin = "http://127.0.0.1:9/v1".to_owned();
    let context = EgressContext::new(
        INSTANCE,
        &dead_origin,
        CredentialKind::GatewayBearer(GW_TOKEN.into()),
    )
    .unwrap();
    let proxy = listener::start_proxy(
        &state_h,
        INSTANCE,
        &HostRelayBind {
            addr: "127.0.0.1:0".into(),
            allow_from: Vec::new(),
        },
        context,
        bearer,
    )
    .await
    .unwrap();
    let endpoint = format!("http://127.0.0.1:{}", proxy.local_addr().port());

    let provision = listener::provision_worker_with_bearer(
        &state_w,
        INSTANCE,
        &dead_origin,
        &via(ApiRouteMode::DirectNet),
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
    assert_eq!(response.status(), 504);
    let body = response.text().await.unwrap();
    assert!(body.contains("upstream-timeout"), "{body}");
    assert!(
        !body.contains("via-host-offline") && !body.contains("127.0.0.1"),
        "wrong code and no origin leak: {body}"
    );
    state_w.revoke_instance(INSTANCE);
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
        CredentialKind::ApiKey("sk-fake-bind-policy-placeholder".into()),
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
                &tokio::sync::Notify::new(),
                tokio::time::Instant::now() + Duration::from_secs(30),
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
                    &tokio::sync::Notify::new(),
                    tokio::time::Instant::now() + Duration::from_secs(30),
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

// ── api.egress credential handoff ──────────────────────────────────────────

/// Start one worker-side stream on the W broker and send `api.open` for
/// POST /messages. Returns the opened stream id and outbox plus the event
/// receiver the caller drains.
async fn open_messages_stream(
    broker: &Arc<LinkBroker>,
) -> (
    String,
    super::Outbox,
    tokio::sync::mpsc::Receiver<InboundEvent>,
) {
    let (stream_id, outbox, events) = broker.open_worker_stream(INSTANCE).unwrap();
    outbox
        .send_notification(
            METHOD_API_OPEN,
            &ApiOpenParams {
                instance_id: INSTANCE.into(),
                stream_id: stream_id.clone(),
                method: "POST".into(),
                path: "/messages".into(),
                query: String::new(),
                headers: vec![],
                body_base64: Some(BASE64.encode(b"{}")),
                body_chunked: false,
                deadline_ms: 30_000,
            },
        )
        .await
        .unwrap();
    (stream_id, outbox, events)
}

/// Pump outbound frames between the two brokers like the Hub would.
fn pump_pair(
    state_w: &Arc<ApiRelayState>,
    state_h: &Arc<ApiRelayState>,
) -> (Arc<LinkBroker>, Arc<LinkBroker>, JoinHandle<()>) {
    let (broker_w, mut rx_w) = state_w.attach_link();
    let (broker_h, mut rx_h) = state_h.attach_link();
    let (w, h) = (Arc::clone(&broker_w), Arc::clone(&broker_h));
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = rx_w.recv() => {
                    let Some(frame) = frame else { break };
                    h.handle_frame(&frame).await;
                }
                frame = rx_h.recv() => {
                    let Some(frame) = frame else { break };
                    w.handle_frame(&frame).await;
                }
            }
        }
    });
    (broker_w, broker_h, task)
}

#[tokio::test]
async fn api_open_before_egress_is_refused_after_install_succeeds_and_revoke_ends() {
    // Script order: the refused open (step 1) never reaches the gateway, so
    // request #1 is step 3 (fast success) and request #2 is step 4 — held at
    // the head for 30 s so revoke lands while the stream is in flight.
    let gateway = FakeGateway::start_with(vec![
        Script::default_messages(),
        Script::slow_first_byte(Duration::from_secs(30)),
    ])
    .await
    .unwrap();
    gateway.expect_credential(GW_TOKEN);
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let (broker_w, broker_h, pump) = pump_pair(&state_w, &state_h);

    // 1. api.open before any api.egress: refused with destination-refused.
    let (_id, _outbox, mut events) = open_messages_stream(&broker_w).await;
    let end = recv_terminal(&mut events).await;
    assert_eq!(
        end.error.as_ref().map(|error| error.code.as_str()),
        Some("destination-refused"),
        "no context installed yet"
    );

    // 2. The context arrives as an api.egress notification — as a frame, the
    //    way the Hub sends it after route decision / reconnect.
    broker_h
        .handle_frame(&json!({
            "jsonrpc": "2.0",
            "method": super::METHOD_API_EGRESS,
            "params": {
                "instanceId": INSTANCE,
                "profileId": "prv_fake",
                "baseUrl": gateway.base_url_v1(),
                "headers": [{"name": "x-profile-header", "value": "profile-value"}],
                "authToken": GW_TOKEN,
                "revoke": false,
            }
        }))
        .await;

    // 3. An api.open after install reaches the gateway: api.head 200.
    let (_id, _outbox, mut events) = open_messages_stream(&broker_w).await;
    let mut saw_head = false;
    let end = recv_terminal_while(&mut events, |event| {
        if let InboundEvent::Head(head) = event {
            assert_eq!(head.status, 200);
            saw_head = true;
        }
    })
    .await;
    assert!(saw_head, "egress served the request after install");
    assert!(end.error.is_none(), "clean end: {end:?}");
    gateway
        .assert_presented_expected_credential()
        .expect("the egress credential came from api.egress");

    // 4. revoke on a still-in-flight stream ends it with an api.end.
    let (_id, _outbox, mut events) = open_messages_stream(&broker_w).await;
    broker_h
        .handle_frame(&json!({
            "jsonrpc": "2.0",
            "method": super::METHOD_API_EGRESS,
            "params": {"instanceId": INSTANCE, "revoke": true}
        }))
        .await;
    let end = recv_terminal(&mut events).await;
    assert!(end.error.is_some(), "revoke ends a live stream: {end:?}");

    // 5. After revoke the context is gone: the next open is refused again.
    let (_id, _outbox, mut events) = open_messages_stream(&broker_w).await;
    let end = recv_terminal(&mut events).await;
    assert_eq!(
        end.error.as_ref().map(|error| error.code.as_str()),
        Some("destination-refused"),
        "revoke clears the in-memory context"
    );

    pump.abort();
}

/// Wait for the terminal `api.end` of a stream.
async fn recv_terminal(
    events: &mut tokio::sync::mpsc::Receiver<InboundEvent>,
) -> remuda_protocol::hubnode::ApiEndParams {
    recv_terminal_while(events, |_| {}).await
}

/// Wait for the terminal frame, running `on_event` for every earlier frame.
async fn recv_terminal_while(
    events: &mut tokio::sync::mpsc::Receiver<InboundEvent>,
    mut on_event: impl FnMut(&InboundEvent),
) -> remuda_protocol::hubnode::ApiEndParams {
    tokio::time::timeout(Duration::from_secs(10), async move {
        while let Some(event) = events.recv().await {
            if let InboundEvent::End(end) = event {
                return end;
            }
            on_event(&event);
        }
        panic!("stream ended without an api.end frame");
    })
    .await
    .expect("api.end within the timeout")
}

// ── credits / hard cap ─────────────────────────────────────────────────────

#[tokio::test]
async fn credit_starved_egress_ends_at_the_hard_cap_after_four_chunks() {
    // A reader on W that stops acknowledging once the initial four-chunk
    // window is drained must not leave the egress parked holding the gateway
    // connection: the fifth gated send races the stream's hard cap (tuned to
    // one second here) and the stream ends upstream-timeout, exactly four
    // chunks having left H. The test stands in for a W that never sends an
    // api.credit.
    let gateway = FakeGateway::start_with(vec![Script::messages("a".repeat(128 * 1024))])
        .await
        .unwrap();
    gateway.expect_credential(GW_TOKEN);
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let tuned = EgressTimeouts {
        ttft: Duration::from_secs(10),
        idle: Duration::from_secs(30),
        hard: Duration::from_secs(1),
    };
    state_h.set_egress_context(gateway_context(&gateway, Some((512, tuned))));
    let (broker_w, _broker_h, pump) = pump_pair(&state_w, &state_h);

    // Open from W and then deliberately never answer a chunk with a credit.
    let (_id, _outbox, mut events) = open_messages_stream(&broker_w).await;

    let mut chunks = 0usize;
    let end = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.expect("event") {
                InboundEvent::Chunk(_) => chunks += 1,
                InboundEvent::End(end) => return end,
                InboundEvent::Head(_) | InboundEvent::Credit(_) => {}
                other => panic!("unexpected event {other:?}"),
            }
        }
    })
    .await
    .expect("the hard cap ends the starved stream");
    pump.abort();

    assert_eq!(chunks, 4, "only the initial window drains without credits");
    let error = end.error.expect("an error end");
    assert_eq!(
        error.code,
        remuda_protocol::hubnode::API_ERROR_UPSTREAM_TIMEOUT,
        "{error:?}"
    );
}

#[tokio::test]
async fn delayed_credits_keep_a_long_response_clean_to_its_last_chunk() {
    // Regression: the EventRouter used to be aborted as soon as the upstream
    // body loop ended, before the tail flush and the zero-byte last chunk were
    // sent. Both sends need permits, and the router was the only thing that
    // granted them from inbound api.credit frames, so a response several times
    // the window long whose W reader drains with a lag parked on its terminal
    // sends until the hard cap. Here the W-side consumer credits one chunk at a
    // time after a short delay; the stream must end cleanly (error None) well
    // inside the tuned cap.
    // The fixture wraps every five text chars in an SSE event, so 16 KiB of
    // text serializes to well over a dozen 8 KiB coalesced chunks — several
    // credit windows.
    let gateway = FakeGateway::start_with(vec![Script::messages("a".repeat(16 * 1024))])
        .await
        .unwrap();
    gateway.expect_credential(GW_TOKEN);
    let state_w = ApiRelayState::new();
    let state_h = ApiRelayState::new();
    let tuned = EgressTimeouts {
        ttft: Duration::from_secs(10),
        idle: Duration::from_secs(10),
        hard: Duration::from_secs(5),
    };
    state_h.set_egress_context(gateway_context(&gateway, Some((8 * 1024, tuned))));
    let (broker_w, _broker_h, pump) = pump_pair(&state_w, &state_h);

    let (_id, outbox, mut events) = open_messages_stream(&broker_w).await;

    let started = std::time::Instant::now();
    let mut chunks = 0usize;
    let mut seqs: Vec<u32> = Vec::new();
    let end = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = events.recv().await {
            match event {
                InboundEvent::Chunk(chunk) => {
                    seqs.push(chunk.seq);
                    chunks += 1;
                    // Drain like a slightly lagging W harness: the tail must
                    // still receive the final credits after upstream ends.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    outbox
                        .send_notification(
                            METHOD_API_CREDIT,
                            &remuda_protocol::hubnode::ApiCreditParams {
                                stream_id: outbox.stream_id().to_owned(),
                                chunks: 1,
                            },
                        )
                        .await
                        .unwrap();
                }
                InboundEvent::End(end) => return end,
                InboundEvent::Head(_) | InboundEvent::Credit(_) => {}
                other => panic!("unexpected event {other:?}"),
            }
        }
        panic!("stream ended without api.end");
    })
    .await
    .expect("clean end within the test timeout");
    let elapsed = started.elapsed();
    pump.abort();

    assert!(end.error.is_none(), "clean terminal end: {:?}", end.error);
    assert!(
        chunks > 12,
        "many windows drained with delayed credits: {chunks}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "ended well inside the 5 s hard cap (took {elapsed:?})"
    );
    // The zero-byte last chunk is present exactly once at the final seq.
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seqs: {seqs:?}"
    );
}
