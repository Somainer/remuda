//! Hub e2e for the D-047/D-048 model-API relay, driven against the offline
//! fake gateway in `remuda-testing`.
//!
//! The shape under test is the in-band one from `docs/design/api-routing.md`
//! §3: a worker's Node accepts a request on its per-instance loopback listener
//! and forwards it as `api.open` / `api.body` over the **existing** Hub↔Node
//! link; the Hub (or the proxy host's Node, when the proxy host is not the Hub
//! host) rebuilds the request against the profile's pinned origin, swaps the
//! credential, and streams the response back as `api.head` / `api.chunk` /
//! `api.end`. Neither leg opens a listener off loopback.
//!
//! ## Why most of this file is `#[ignore]`d
//!
//! The relay router is task 2 (`c-apiroute-hub`). Until it lands, an `api.*`
//! method sent Node→Hub has no arm and the Hub answers JSON-RPC `-32602`
//! `"unknown method api.open"` (see `crates/remuda-hub/src/ws.rs`, the
//! `other =>` fall-through of `handle_node_method`). Every assertion that needs
//! the router is therefore written out in full and marked
//! `#[ignore = "relay router lands with c-apiroute-hub"]` — that exact wording,
//! so `grep -c 'relay router lands with c-apiroute-hub'` lists every one to
//! un-ignore when the router merges. This file then becomes the acceptance
//! evidence, and until then it records, in executable form, exactly what "done"
//! means rather than a prose restatement of it.
//!
//! The three tests that are **not** ignored need no relay: they pin the two
//! facts the relay will be built on, both of which would silently invalidate
//! the design if they changed —
//!
//! 1. an unknown `api.*` method is refused as a bad method today (so the
//!    ignored tests are not failing for the wrong reason, and the day this
//!    flips the un-ignored set is the reminder the router arrived);
//! 2. the relay's destination really is a working Anthropic-Messages origin,
//!    streamed over the same loopback path a relay would take.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use remuda_testing::fake_gateway::{DEFAULT_TEXT, FakeGateway, Script};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Generous: this box is a shared devbox and the Hub is spun up in-process.
const TIMEOUT: Duration = Duration::from_secs(20);

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn recv_json(ws: &mut NodeSocket) -> Result<Value> {
    loop {
        let message = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected frame {other:?}")),
        }
    }
}

async fn send_json(node: &mut NodeSocket, frame: Value) -> Result<()> {
    node.send(Message::Text(frame.to_string().into())).await?;
    Ok(())
}

/// Send a request and read frames until the matching `id` comes back.
///
/// Any notification that arrives first (an `api.*` relay frame, in the
/// un-ignored future) is handed to `collect` rather than dropped.
async fn request<F>(
    node: &mut NodeSocket,
    frame: Value,
    mut collect: F,
) -> Result<(Value, Vec<Value>)>
where
    F: FnMut(&Value) -> bool,
{
    let rpc_id = frame["id"].clone();
    send_json(node, frame).await?;
    let mut notifications = Vec::new();
    loop {
        let reply = recv_json(node).await?;
        if reply.get("method").is_some() {
            if collect(&reply) {
                notifications.push(reply["params"].clone());
            }
            continue;
        }
        if reply["id"] == rpc_id {
            return Ok((reply, notifications));
        }
    }
}

/// A booted Hub plus a connected fake Node, held alive for the test's duration.
struct Fixture {
    addr: std::net::SocketAddr,
    cookie: String,
    node: NodeSocket,
    instance_id: String,
    _hub: remuda_hub::RunningHub,
    _dir: tempfile::TempDir,
}

/// Boot a Hub, mint an enroll token, connect a fake Node, and project one
/// running instance onto it.
async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;

    // Note: `upgrade`, not `request` — `request` is the RPC helper above.
    let mut upgrade = format!("ws://{addr}/v1/node").into_client_request()?;
    upgrade
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(upgrade)).await??;

    let host_id = HostId::new().as_id().as_str().to_owned();
    send_json(
        &mut node,
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.1.0", "label":"relay-node"}}),
    )
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.pointer("/result/hostId").is_some(), "{hello}");

    // Project a running instance so the relay has an instance id to authorize
    // against (the Hub re-checks the Node's word, exactly as for object.pull).
    let instance_id = InstanceId::new().as_id().as_str().to_owned();
    let (ack, _) = request(
        &mut node,
        json!({"jsonrpc":"2.0", "id":"s1", "method":"journal.append",
            "params":{"instanceId":instance_id, "event":{
                "kind":"lifecycle",
                "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                           "reasonCode":"driver-started"}}}}),
        |_| false,
    )
    .await?;
    anyhow::ensure!(ack.get("result").is_some(), "instance projection: {ack}");

    Ok(Fixture {
        addr,
        cookie,
        node,
        instance_id,
        _hub: hub,
        _dir: dir,
    })
}

/// Minimal HTTP client: one request, `Connection: close`, head and body back.
///
/// Returns `(status, head, body)` — the head is separate because the login
/// cookie only exists there, and the body because every other route answers in
/// JSON.
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(body) = body {
        request.extend_from_slice(body.as_bytes());
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "api-relay-test" }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':')?.1.split(';').next())
        .map(|value| value.trim().to_string())
        .context("login set-cookie")
}

/// Create a gateway profile pointing at `base_url` and return its id.
///
/// The token is synthesized per call and never asserted on: the fake gateway
/// records header *names*, so a test proves the credential was attached without
/// any real credential existing (D-047 §Secrets).
async fn create_profile(
    addr: std::net::SocketAddr,
    cookie: &str,
    base_url: &str,
) -> Result<String> {
    let body = json!({
        "name": "relay-fixture",
        "kind": "gateway",
        "baseUrl": base_url,
        "models": ["fake/model-1"],
        "defaultModel": "fake/model-1",
        "authToken": "sk-relay-fixture-0000",
        "defaultGateway": true,
    })
    .to_string();
    let (status, _, rest) = http(
        addr,
        "POST",
        "/v1/providers",
        &[("Cookie", cookie)],
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "create profile {status} {rest}");
    let created: Value = serde_json::from_str(rest.trim())?;
    created["id"]
        .as_str()
        .map(str::to_string)
        .context("profile id")
}

/// The `api.open` params a worker's Node would send for one Messages request.
///
/// Built as raw JSON rather than from the protocol type on purpose: this file
/// must compile against the base commit, and the Hub parses incoming node
/// frames as `serde_json::Value` anyway. The `#[ignore]`d tests are the ones
/// that will exercise it; when `c-apiroute-hub` lands, swapping these for
/// `remuda_protocol::ApiOpenParams` is a local change confined to this helper.
fn api_open_params(instance_id: &str, stream_id: &str, body: &str) -> Value {
    json!({
        "instanceId": instance_id,
        "streamId": stream_id,
        "method": "POST",
        "path": "messages",
        "query": "",
        "headers": [
            { "name": "content-type", "value": "application/json" },
            { "name": "anthropic-version", "value": "2023-06-01" },
        ],
        "bodyBase64": base64_encode(body.as_bytes()),
        "bodyChunked": false,
        "deadlineMs": 30_000,
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Reassemble the `text_delta` payloads of an SSE body into one string.
///
/// Returns an error on a malformed frame rather than skipping it: a frame the
/// relay mangled is exactly what the caller is looking for.
fn reassemble_deltas(body: &str) -> Result<String> {
    let mut out = String::new();
    for block in body.split("\n\n") {
        let mut event = None;
        let mut data = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = Some(rest);
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = Some(rest);
            }
        }
        if event != Some("content_block_delta") {
            continue;
        }
        let value: Value = serde_json::from_str(data.context("delta without data")?)?;
        out.push_str(
            value
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
    }
    Ok(out)
}

/// Reassemble the `api.chunk` notifications of one stream, in `seq` order.
///
/// Returns the decoded body and the terminal `api.end` params, so a caller can
/// assert on both the bytes and the counters (which is all the journal is
/// allowed to carry — never a body or a header).
fn collect_stream(chunks: &[Value]) -> Result<(Vec<u8>, Option<Value>)> {
    let mut ordered: Vec<&Value> = chunks
        .iter()
        .filter(|frame| frame.get("dataBase64").is_some())
        .collect();
    ordered.sort_by_key(|frame| frame["seq"].as_u64().unwrap_or(0));
    let mut body = Vec::new();
    for chunk in &ordered {
        use base64::Engine;
        let raw = chunk["dataBase64"].as_str().context("dataBase64")?;
        body.extend_from_slice(&base64::engine::general_purpose::STANDARD.decode(raw)?);
    }
    let end = chunks
        .iter()
        .find(|frame| frame.get("bytesDown").is_some())
        .cloned();
    Ok((body, end))
}

/// A Messages request body, as a client would send it.
fn messages_body() -> String {
    json!({
        "model": "fake/model-1",
        "max_tokens": 64,
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    })
    .to_string()
}

// ── Un-ignored: the two facts the relay is built on ────────────────────────

/// The refused-method probe that keeps the ignored tests honest.
///
/// Until the router lands, `api.open` is simply an unknown method. This test
/// pins that, so the `#[ignore]`s below are not silently passing for the wrong
/// reason — and the day this flips to a real result, the un-ignored tests are
/// the reminder that the router arrived.
#[tokio::test]
async fn api_open_is_not_a_known_method_until_the_relay_router_lands() -> Result<()> {
    let mut fixture = fixture().await?;
    let (reply, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"probe", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_probe", &messages_body())}),
        |_| false,
    )
    .await?;

    if reply.get("error").is_none() {
        // The router landed: this probe must be replaced by the real relay
        // assertions below, which is why they exist.
        panic!("api.open is now a known method — un-ignore the relay tests: {reply}");
    }
    assert_eq!(
        reply["error"]["code"], -32602,
        "an unknown node method is a bad-method refusal: {reply}"
    );
    let message = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("api.open"),
        "the refusal must name the method: {message}"
    );
    Ok(())
}

/// The relay's destination is a real, streaming Messages origin.
///
/// This is the one test that exercises the fake gateway over the same loopback
/// path a relay takes, and it needs no Hub relay code: it speaks HTTP to the
/// gateway directly. It fails if the fixture and the relay disagree about what
/// the origin does — SSE framing, the `anthropic-version` header, or the
/// two-catalog listing.
#[tokio::test]
async fn the_relay_destination_streams_sse_and_lists_two_catalogs() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let client = reqwest::Client::builder().timeout(TIMEOUT).build()?;

    // A credential-free request still streams, which is what lets the ignored
    // relay tests assert the credential *swap* rather than the credential.
    let response = client
        .post(format!("{}/v1/messages", gateway.base_url()))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(messages_body())
        .send()
        .await
        .context("POST /v1/messages")?;
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream")),
        "the origin must stream: {:?}",
        response.headers()
    );
    let body = response.text().await?;
    assert!(body.contains("event: message_start"), "{body}");
    assert!(body.contains("event: message_stop"), "{body}");
    // The text arrives split across `text_delta` events, so the way to check it
    // is to reassemble the deltas — which is also what proves the framing is
    // lossless rather than merely well-formed.
    assert_eq!(reassemble_deltas(&body)?, DEFAULT_TEXT);

    // The listing differs per header, so a discovery probe that read one
    // surface would under-report.
    let plain: Value = client
        .get(format!("{}/v1/models", gateway.base_url()))
        .send()
        .await?
        .json()
        .await?;
    let anthropic: Value = client
        .get(format!("{}/v1/models", gateway.base_url()))
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?
        .json()
        .await?;
    let ids = |value: &Value| -> Vec<String> {
        value["data"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_ne!(
        ids(&plain),
        ids(&anthropic),
        "the fixture must reproduce the two-listings behaviour"
    );

    // A discovery probe through the Hub unions both surfaces, so the profile
    // the relay tests create resolves against a catalog with both sets of ids.
    let (hub, bootstrap, _dir) = {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let bootstrap = hub.bootstrap_token.clone();
        (hub, bootstrap, dir)
    };
    let cookie = login(hub.addr, &bootstrap).await?;
    let discover = json!({ "baseUrl": gateway.base_url() }).to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &[("Cookie", cookie.as_str())],
        Some(&discover),
    )
    .await?;
    anyhow::ensure!(status == 200, "discover {status} {rest}");
    let found: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(found["ok"], true, "{found}");
    // `/discover` answers its own shape: the catalog sits under `models`.
    let discovered: Vec<String> = found["models"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for id in ids(&plain).iter().chain(ids(&anthropic).iter()) {
        assert!(
            discovered.contains(id),
            "the union must offer {id}; got {discovered:?}"
        );
    }

    gateway.assert_no_credentials()?;
    gateway.shutdown().await;
    Ok(())
}

/// A profile pointing at the fixture is accepted, and `POST /test` reaches it.
///
/// The relay tests all need a stored profile whose `baseUrl` is the fixture;
/// this proves that part works today, independently of the router.
#[tokio::test]
async fn a_profile_can_point_at_the_fake_gateway_and_test_it() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token.clone()).await?;

    let id = create_profile(hub.addr, &cookie, &gateway.base_url()).await?;
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        &format!("/v1/providers/{id}/test"),
        &[("Cookie", cookie.as_str())],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "provider test {status} {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["reachable"], true, "{result}");

    // The credential reached the origin — recorded as a name, never a value —
    // and nothing the fixture holds could leak it.
    assert!(
        gateway.saw_header("authorization") || gateway.saw_header("x-api-key"),
        "the probe must present a credential: {:?}",
        gateway.header_names()
    );
    gateway.shutdown().await;
    Ok(())
}

// ── Ignored until `c-apiroute-hub` lands: the relay's own acceptance ───────

/// Happy path: one relayed Messages request, streamed back to the worker.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn relays_a_messages_request_and_streams_the_response_back() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture().await?;
    let profile_id = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;
    let _ = profile_id;

    let (reply, frames) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_1", &messages_body())}),
        |frame| {
            matches!(
                frame["method"].as_str(),
                Some("api.head" | "api.chunk" | "api.end")
            )
        },
    )
    .await?;
    assert!(reply.get("error").is_none(), "api.open refused: {reply}");

    // The response head arrives before the body, so the worker's listener can
    // commit a status line without waiting for the first byte.
    let head = frames
        .iter()
        .find(|frame| frame.get("status").is_some())
        .context("no api.head")?;
    assert_eq!(head["status"], 200);

    let (body, end) = collect_stream(&frames)?;
    let body = String::from_utf8(body)?;
    assert!(body.contains("event: message_start"), "{body}");
    assert!(body.contains("event: content_block_delta"), "{body}");
    assert!(body.contains("event: message_stop"), "{body}");
    // Reassembled across chunks, the deltas still spell the scripted text —
    // coalescing preserves byte order exactly (D-048).
    assert_eq!(reassemble_deltas(&body)?, DEFAULT_TEXT);

    // Counters only: no body and no header ever reaches the audit record.
    let end = end.context("no api.end")?;
    assert!(end.get("error").is_none(), "stream ended in error: {end}");
    assert!(
        end["bytesDown"].as_u64().unwrap_or(0) >= body.len() as u64,
        "api.end must count the bytes relayed: {end}"
    );
    assert!(
        end["ms"].as_u64().is_some(),
        "api.end carries a duration: {end}"
    );
    assert!(
        end.get("bodyBase64").is_none() && end.get("headers").is_none(),
        "api.end must carry counters, never a body or a header: {end}"
    );

    // The credential swap happened at the origin: the relay attached the
    // profile credential, and the worker's relay bearer never went upstream.
    assert!(
        gateway.saw_header("authorization") || gateway.saw_header("x-api-key"),
        "the origin must see the profile credential: {:?}",
        gateway.header_names()
    );
    assert!(
        gateway.saw_header("anthropic-version"),
        "the request header allowlist keeps anthropic-version: {:?}",
        gateway.header_names()
    );
    // The fixture is the only thing that could leak, and it holds no value.
    gateway.assert_no_credentials()?;

    gateway.shutdown().await;
    Ok(())
}

/// An oversized request body is chunked as `api.body` frames and reassembled.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn relays_a_chunked_request_body_larger_than_one_frame() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    // Big enough to need several 64 KiB chunks, under the 1 MiB frame cap.
    let filler = "x".repeat(200 * 1024);
    let body = json!({
        "model": "fake/model-1",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": filler }],
    })
    .to_string();

    // Open with no inline body and `bodyChunked`, then send the body as
    // `api.body` frames — one per 64 KiB slice, `last` on the final one.
    let mut params = api_open_params(&fixture.instance_id, "st_chunked", "");
    params["bodyChunked"] = json!(true);
    send_json(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open", "params": params}),
    )
    .await?;

    const CHUNK: usize = 64 * 1024;
    let slices: Vec<&[u8]> = body.as_bytes().chunks(CHUNK).collect();
    assert!(slices.len() >= 3, "a 200 KiB body must need several chunks");
    for (seq, slice) in slices.iter().enumerate() {
        let last = seq + 1 == slices.len();
        let (ack, _) = request(
            &mut fixture.node,
            json!({"jsonrpc":"2.0", "id": format!("body{seq}"), "method":"api.body",
                "params":{"streamId":"st_chunked", "seq": seq,
                          "dataBase64": base64_encode(slice), "last": last}}),
            |_| false,
        )
        .await?;
        assert!(ack.get("error").is_none(), "api.body {seq} refused: {ack}");
    }

    // The origin received the whole body, reassembled in order — which is the
    // only thing that proves the chunking was lossless rather than merely
    // accepted frame by frame.
    let received = gateway
        .requests_to("/v1/messages")
        .first()
        .map(|request| request.body_bytes)
        .context("the relayed request never reached the origin")?;
    assert_eq!(
        received,
        body.len(),
        "the origin must see the reassembled body length"
    );
    gateway.shutdown().await;
    Ok(())
}

/// H going offline mid-stream ends the stream truthfully — never by rerouting.
///
/// D-047 §Failure behaviour: falling back to direct delivery would leak the
/// request to a machine the operator excluded and make the session strip lie,
/// so the stream must end with an error and `remuda watch` must report
/// `api-route-down`.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn a_via_host_lost_mid_stream_ends_the_stream_and_never_reroutes() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;
    // Taken out before the link is dropped, so the observe call below can still
    // reach the Hub.
    let addr = fixture.addr;
    let cookie = fixture.cookie.clone();

    let (reply, frames) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_offline", &messages_body())}),
        |_| false,
    )
    .await?;
    assert!(reply.get("error").is_none(), "api.open refused: {reply}");

    // Drop the proxy host's link, then drive the failure the way the Hub sees
    // it: the link dies with a stream in flight.
    drop(fixture.node);

    // The worker's listener must answer a real API error (503, Anthropic
    // shaped), not hang and not silently reach the origin by another path.
    let (_status, end) = collect_stream(&frames)?;
    let end = end.context("the stream must be terminated, not abandoned")?;
    assert_eq!(
        end["error"]["code"], "via-host-offline",
        "the terminal frame must name the cause: {end}"
    );

    // The origin saw exactly what it saw before the failure — no second,
    // direct request appeared behind the operator's back. A direct fallback
    // would also have to present a credential to the origin, which is the one
    // thing the relay is built to prevent.
    assert!(
        !gateway.saw_header("x-remuda-direct-fallback"),
        "no code path may construct a direct request once mode == via"
    );
    let before = gateway.request_count();
    assert_eq!(
        before,
        0,
        "a lost via host must not push the request anywhere else: {:?}",
        gateway.requests()
    );

    // And the roster observation becomes the reason `remuda watch` reports.
    // `API_ROUTE_DOWN` is a worker-watch reason, not an instance field, so the
    // surface is `POST /v1/workers/observe` — a GET of the instance is not it.
    let (status, _, rest) = http(
        addr,
        "POST",
        "/v1/workers/observe",
        &[("Cookie", cookie.as_str())],
        Some("{}"),
    )
    .await?;
    assert_eq!(status, 200, "observe {status} {rest}");
    let observed: Value = serde_json::from_str(rest.trim())?;
    let reason = observed["workers"]
        .as_array()
        .map(|workers| {
            workers
                .iter()
                .filter_map(|worker| worker["reason"].as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        reason.contains(&"api-route-down"),
        "a lost route is reported, not hidden: {observed}"
    );

    gateway.shutdown().await;
    Ok(())
}

/// An oversized single frame is refused rather than truncated.
///
/// D-048: `apiChunkBytes` is 64 KiB raw, far under the 1 MiB
/// `maxJsonFrameBytes`. A producer that exceeds it must be told, not silently
/// cut off mid-body.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn an_oversized_frame_is_refused_not_truncated() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    // A single chunk several times the 64 KiB cap.
    let oversized = base64_encode(&vec![b'x'; 4 * 1024 * 1024]);
    let (reply, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_big", &messages_body())}),
        |_| false,
    )
    .await?;
    assert!(reply.get("error").is_none(), "api.open refused: {reply}");

    let (chunk_reply, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"chunk", "method":"api.chunk",
            "params":{"streamId":"st_big", "seq":0, "dataBase64": oversized, "last":false}}),
        |_| false,
    )
    .await?;
    // Either a JSON-RPC refusal or an `api.end{error}` — never a silent accept.
    let refused = chunk_reply.get("error").is_some()
        || chunk_reply
            .pointer("/result/error/code")
            .and_then(Value::as_str)
            .is_some();
    assert!(
        refused,
        "a frame over apiChunkBytes must be refused, not truncated: {chunk_reply}"
    );

    gateway.shutdown().await;
    Ok(())
}

/// A live relay stream must not starve tty or control frames.
///
/// Risk 1 in the plan: the Hub→Node outbound queue is 32 slots shared with tty
/// frames, so `api.*` uses its own stream table plus per-stream credits rather
/// than the RPC pending map. With a relay stream hot, `instance.create` and
/// `tty.write` must still be serviced promptly.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn tty_frames_still_flow_while_a_relay_stream_is_hot() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::messages("x".repeat(512 * 1024))]).await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    // Open a relay stream and leave it running.
    send_json(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_hot", &messages_body())}),
    )
    .await?;

    // While it is in flight, an ordinary RPC must round-trip inside a tight
    // budget — this is the assertion that fails if `api.*` touched the shared
    // 32-slot pending map.
    let started = std::time::Instant::now();
    let (reply, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"tty", "method":"tty.write",
            "params":{"instanceId": fixture.instance_id, "data":"ls\n"}}),
        |_| false,
    )
    .await?;
    let elapsed = started.elapsed();
    assert!(
        reply.get("error").is_none() || reply.error_code() != Some(-32602),
        "tty.write must be a known method: {reply}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "tty.write waited {elapsed:?} behind a hot relay stream"
    );
    gateway.shutdown().await;
    Ok(())
}

/// A destination outside the profile's pinned origin is refused in both halves.
///
/// Risk 5: the relay must not become a general HTTP proxy. The Node pins origin
/// and path; the Hub re-checks. A request naming another origin must be refused
/// with `destination-refused`, and the origin must never see it.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn a_non_allowlisted_destination_is_refused() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    let mut params = api_open_params(&fixture.instance_id, "st_evil", &messages_body());
    // A path escaping the profile's base path, and an absolute origin.
    params["path"] = json!("../../etc/passwd");
    let (reply, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open1", "method":"api.open", "params": params}),
        |_| false,
    )
    .await?;
    assert!(
        reply.get("error").is_some(),
        "a path outside the base path must be refused: {reply}"
    );

    let mut params = api_open_params(&fixture.instance_id, "st_evil2", &messages_body());
    params["path"] = json!("messages");
    params["headers"] = json!([{ "name": "host", "value": "elsewhere.example:443" }]);
    let (reply, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open2", "method":"api.open", "params": params}),
        |_| false,
    )
    .await?;
    assert!(
        reply.get("error").is_some(),
        "a request naming another origin must be refused: {reply}"
    );

    // The pinned origin saw nothing from either attempt.
    assert_eq!(
        gateway.request_count(),
        0,
        "a refused destination must never reach the gateway: {:?}",
        gateway.requests()
    );
    gateway.shutdown().await;
    Ok(())
}

/// Credit exhaustion stalls the producer instead of filling the shared queue.
///
/// D-048: a producer may have at most 4 unacked chunks per stream and stalls at
/// the cap; the consumer sends `api.credit` as it drains. Without this, a long
/// SSE stream would fill the 32-slot outbound queue and block tty frames.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn a_producer_stalls_at_the_credit_cap() -> Result<()> {
    // A response long enough to need far more chunks than the credit cap.
    let gateway =
        FakeGateway::start_with(vec![Script::messages("y".repeat(4 * 1024 * 1024))]).await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    send_json(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_credit", &messages_body())}),
    )
    .await?;

    // Read whatever arrives without crediting, and count it. The producer must
    // stop at the cap rather than streaming the whole body into the queue.
    let mut uncredited = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), recv_json(&mut fixture.node)).await {
            Ok(Ok(frame)) if frame["method"] == "api.chunk" => uncredited += 1,
            Ok(Ok(_)) => continue,
            Ok(Err(err)) => return Err(err),
            // Silence is the expected steady state once credits run out.
            Err(_) => break,
        }
        if uncredited > 4 {
            break;
        }
    }
    // Both halves matter: at least one chunk must have arrived (the stream
    // really started) and no more than the cap must have (it really stalled).
    // Asserting only the upper bound would pass on a stream that never ran.
    assert!(
        uncredited >= 1,
        "the relay stream must start before it can stall at a cap"
    );
    assert!(
        uncredited <= 4,
        "the producer must stall at the credit cap, saw {uncredited} uncredited chunks"
    );

    // Credit the stream and the producer resumes.
    send_json(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "method":"api.credit",
            "params":{"streamId":"st_credit", "chunks": 4}}),
    )
    .await?;
    let resumed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = recv_json(&mut fixture.node).await?;
            if frame["method"] == "api.chunk" {
                return Ok::<Value, anyhow::Error>(frame);
            }
        }
    })
    .await
    .context("the producer must resume after a credit")??;
    assert_eq!(resumed["method"], "api.chunk");

    gateway.shutdown().await;
    Ok(())
}

/// A `429` observed at the origin reaches the supply-evidence path as a status.
///
/// D-047 §Journal: the Hub projects observed `429`/`529` from the stream into
/// the existing supply evidence, which upgrades rate-limit detection from
/// screen-scraping to real HTTP status codes.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn an_origin_429_is_projected_into_supply_evidence() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::status(429)]).await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    let (reply, frames) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_429", &messages_body())}),
        |frame| frame["method"] == "api.head" || frame["method"] == "api.end",
    )
    .await?;
    assert!(reply.get("error").is_none(), "api.open refused: {reply}");

    // The status is relayed verbatim, so the worker's listener answers a real
    // Anthropic rate-limit error rather than a transport fault.
    let head = frames
        .iter()
        .find(|frame| frame.get("status").is_some())
        .context("no api.head")?;
    assert_eq!(head["status"], 429, "{head}");
    let has_retry_after = head["headers"]
        .as_array()
        .map(|headers| {
            headers.iter().any(|header| {
                header["name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("retry-after"))
            })
        })
        .unwrap_or(false);
    assert!(
        has_retry_after,
        "retry-after survives the response allowlist: {head}"
    );

    // The terminal frame carries the status the supply projection reads, and no
    // body — the journal must never carry bodies or headers.
    let (_, end) = collect_stream(&frames)?;
    let end = end.context("no api.end")?;
    assert!(
        end["status"].as_u64() == Some(429) || end["error"].is_object(),
        "api.end must carry the observed status or an error code: {end}"
    );
    assert!(
        end.get("headers").is_none() && end.get("bodyBase64").is_none(),
        "counters only: {end}"
    );
    gateway.shutdown().await;
    Ok(())
}

/// The response allowlist drops `set-cookie` and keeps `anthropic-*`.
#[tokio::test]
#[ignore = "relay router lands with c-apiroute-hub"]
async fn the_response_header_allowlist_drops_set_cookie() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture().await?;
    let _ = create_profile(fixture.addr, &fixture.cookie, &gateway.base_url()).await?;

    let (reply, frames) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"open", "method":"api.open",
            "params": api_open_params(&fixture.instance_id, "st_hdr", &messages_body())}),
        |frame| frame["method"] == "api.head",
    )
    .await?;
    assert!(reply.get("error").is_none(), "api.open refused: {reply}");

    let head = frames.first().context("no api.head")?;
    let names: Vec<String> = head["headers"]
        .as_array()
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| header["name"].as_str().map(str::to_ascii_lowercase))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !names.iter().any(|name| name == "set-cookie"),
        "set-cookie is always dropped: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.starts_with("content-type")),
        "content-type survives: {names:?}"
    );
    gateway.shutdown().await;
    Ok(())
}

/// Helper used by the tty test to inspect a JSON-RPC refusal code.
trait ReplyExt {
    /// The JSON-RPC error code, when the reply is an error.
    fn error_code(&self) -> Option<i64>;
}

impl ReplyExt for Value {
    fn error_code(&self) -> Option<i64> {
        self.pointer("/error/code").and_then(Value::as_i64)
    }
}
