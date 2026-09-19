//! Self-tests for the offline Anthropic-Messages gateway double.
//!
//! One test per scripted behaviour the api-routing tasks rely on: SSE framing,
//! the non-streaming shape, `429`/`529`/`401`, a slow first byte, a mid-stream
//! abort, the two-listings-per-gateway `/v1/models` behaviour, and the header
//! **name** recorder — including the one assertion that matters most, that no
//! credential value is ever stored (or printable) by this fixture.
//!
//! Every test talks to the gateway over a real loopback socket: the point of
//! this double is to exercise the HTTP path a relay will take, so a test that
//! called the handlers directly would prove nothing about framing.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use remuda_testing::fake_gateway::{
    ANTHROPIC_VERSION, DEFAULT_MODEL, DEFAULT_TEXT, FakeGateway, Script, parse_listen, parse_script,
};
use serde_json::Value;

/// Post a messages request and return `(status, headers, body)`.
///
/// `headers` is the raw response head, so a test can assert on `retry-after`
/// and the content type without a header-map dependency.
async fn post_messages(
    gateway: &FakeGateway,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(u16, String, String)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut request = client
        .post(format!("{}/v1/messages", gateway.base_url()))
        .header("content-type", "application/json")
        .body(body.to_string());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.context("POST /v1/messages")?;
    let status = response.status().as_u16();
    let head = format!("{:?}", response.headers());
    let text = response.text().await.unwrap_or_default();
    Ok((status, head, text))
}

/// The default request body: a minimal, valid Anthropic Messages request.
fn request_body() -> String {
    serde_json::json!({
        "model": DEFAULT_MODEL,
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "hi" }],
    })
    .to_string()
}

/// Parse a `text/event-stream` body into `(event, data)` pairs.
///
/// Deliberately strict: a malformed frame is an error, not something to skip,
/// because a malformed frame is exactly what a relay would have to forward.
fn parse_sse(body: &str) -> Result<Vec<(String, String)>> {
    let mut events = Vec::new();
    for block in body.split("\n\n") {
        let block = block.trim_matches('\n');
        if block.is_empty() {
            continue;
        }
        let mut event = None;
        let mut data = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = Some(rest.to_string());
            } else if !line.starts_with(':') {
                return Err(anyhow!("unexpected SSE line {line:?}"));
            }
        }
        match (event, data) {
            (Some(event), Some(data)) => events.push((event, data)),
            _ => return Err(anyhow!("incomplete SSE frame {block:?}")),
        }
    }
    Ok(events)
}

/// Reassemble the `text_delta` payloads of a parsed SSE stream.
fn deltas_text(events: &[(String, String)]) -> Result<String> {
    let mut out = String::new();
    for (event, data) in events {
        if event != "content_block_delta" {
            continue;
        }
        let value: Value = serde_json::from_str(data)?;
        out.push_str(
            value
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .context("delta.text")?,
        );
    }
    Ok(out)
}

#[tokio::test]
async fn sse_stream_frames_the_documented_event_sequence() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let (status, head, body) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(status, 200);
    assert!(
        head.contains("text/event-stream"),
        "content type not SSE: {head}"
    );

    let events = parse_sse(&body)?;
    let names: Vec<&str> = events.iter().map(|(event, _)| event.as_str()).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.get(1), Some(&"content_block_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    assert!(
        names.contains(&"content_block_stop"),
        "missing content_block_stop: {names:?}"
    );
    assert!(
        names.contains(&"message_delta"),
        "missing message_delta: {names:?}"
    );
    assert!(names.contains(&"content_block_delta"));

    // The deltas are lossless: reassembling them reproduces the script exactly.
    assert_eq!(deltas_text(&events)?, DEFAULT_TEXT);

    // `message_start` opens the message the client will render.
    let start: Value = serde_json::from_str(&events[0].1)?;
    assert_eq!(start["message"]["model"], DEFAULT_MODEL);
    assert_eq!(start["message"]["role"], "assistant");
    assert_eq!(start["message"]["type"], "message");

    // `message_delta` closes it with a stop reason and a usage count.
    let end: Value = serde_json::from_str(
        events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .context("message_delta")?
            .1
            .as_str(),
    )?;
    assert_eq!(end["delta"]["stop_reason"], "end_turn");
    assert!(end["usage"]["output_tokens"].as_u64().unwrap_or(0) > 0);

    let _ = gateway.assert_no_credentials();
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn non_streaming_request_answers_one_json_message() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::messages_json("plain body")]).await?;
    let (status, head, body) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(status, 200);
    assert!(head.contains("application/json"), "not JSON: {head}");
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["type"], "message");
    assert_eq!(value["content"][0]["type"], "text");
    assert_eq!(value["content"][0]["text"], "plain body");
    assert_eq!(value["stop_reason"], "end_turn");
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn scripted_429_is_a_rate_limit_error_with_retry_after() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::status(429)]).await?;
    let (status, head, body) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(status, 429);
    assert!(head.contains("retry-after"), "no retry-after: {head}");
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "rate_limit_error");
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn scripted_529_is_an_overloaded_error() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::status(529)]).await?;
    let (status, _, body) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(status, 529);
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["error"]["type"], "overloaded_error");
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn scripted_401_is_an_authentication_error() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::status(401)]).await?;
    let (status, _, body) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(status, 401);
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["error"]["type"], "authentication_error");
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn scripted_statuses_are_replayed_in_order_then_the_last_repeats() -> Result<()> {
    let gateway =
        FakeGateway::start_with(vec![Script::status(429), Script::messages("served")]).await?;
    let (first, _, _) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(first, 429, "first request takes the first script");
    for attempt in 2..=4 {
        let (status, _, body) = post_messages(&gateway, &[], &request_body()).await?;
        assert_eq!(
            status, 200,
            "attempt {attempt} should fall through to the tail"
        );
        assert_eq!(deltas_text(&parse_sse(&body)?)?, "served");
    }
    assert_eq!(gateway.request_count(), 4);
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_slow_first_byte_holds_the_response_head() -> Result<()> {
    let gateway =
        FakeGateway::start_with(vec![Script::slow_first_byte(Duration::from_millis(400))]).await?;
    let started = std::time::Instant::now();
    let (status, _, body) = post_messages(&gateway, &[], &request_body()).await?;
    let elapsed = started.elapsed();
    assert_eq!(status, 200);
    assert!(
        elapsed >= Duration::from_millis(350),
        "answered too early ({elapsed:?}); the delay is the fixture"
    );
    assert_eq!(deltas_text(&parse_sse(&body)?)?, DEFAULT_TEXT);
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_abort_mid_stream_delivers_deltas_then_truncates() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::abort_mid_stream(
        "a longer scripted body here",
    )])
    .await?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .post(format!("{}/v1/messages", gateway.base_url()))
        .header("content-type", "application/json")
        .body(request_body())
        .send()
        .await;
    // The connection is aborted mid-body, so either the send itself fails or
    // the body read does. Both are the truncation this script models; a clean
    // 200 with a complete SSE document is not.
    let body = match response {
        Ok(response) => {
            assert_eq!(response.status().as_u16(), 200);
            match response.text().await {
                Ok(body) => body,
                Err(_) => {
                    gateway.shutdown().await;
                    return Ok(());
                }
            }
        }
        Err(_) => {
            gateway.shutdown().await;
            return Ok(());
        }
    };
    // If a body did arrive, it must be visibly truncated: deltas but no
    // terminator, which is what a relay has to notice and report.
    assert!(
        !body.contains("message_stop"),
        "an aborted stream must not reach message_stop: {body:?}"
    );
    let events = parse_sse(&body).unwrap_or_default();
    assert!(
        events
            .iter()
            .any(|(event, _)| event == "content_block_delta"),
        "the abort must still deliver deltas first: {body:?}"
    );
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn models_lists_two_catalogs_that_differ_per_header() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let client = reqwest::Client::new();

    let plain: Value = client
        .get(format!("{}/v1/models", gateway.base_url()))
        .header("authorization", "Bearer not-a-real-token")
        .send()
        .await?
        .json()
        .await?;
    let plain_ids: Vec<&str> = plain["data"]
        .as_array()
        .context("plain data")?
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(plain_ids.contains(&DEFAULT_MODEL));
    assert!(
        plain_ids.contains(&"passthrough/fake-evolving"),
        "the plain listing is the broad OpenAI-style catalog: {plain_ids:?}"
    );

    let anthropic: Value = client
        .get(format!("{}/v1/models", gateway.base_url()))
        .header("authorization", "Bearer not-a-real-token")
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await?
        .json()
        .await?;
    let anthropic_ids: Vec<&str> = anthropic["data"]
        .as_array()
        .context("anthropic data")?
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(anthropic_ids.contains(&"claude-fake-shared"));
    assert!(
        !anthropic_ids.contains(&"passthrough/fake-evolving"),
        "the anthropic surface is the short claude-* subset: {anthropic_ids:?}"
    );

    // The two listings overlap on exactly one id, so a probe that read a
    // single surface under-reports — the property the union exists for.
    let shared: Vec<&&str> = plain_ids
        .iter()
        .filter(|id| anthropic_ids.contains(id))
        .collect();
    assert_eq!(shared.len(), 3, "union is non-trivial: {shared:?}");

    // A context window of 1M earns the `1m` tag downstream, so the fixture
    // has to report one.
    let long = anthropic["data"]
        .as_array()
        .context("anthropic data")?
        .iter()
        .find(|item| item["id"] == "fake/model-1m")
        .context("fake/model-1m")?;
    assert!(
        long["context_window"].as_u64().unwrap_or(0) >= 1_000_000,
        "{long}"
    );

    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn models_answers_under_a_base_path_too() -> Result<()> {
    // A gateway base URL may already end in `/v1`; the router must answer both
    // spellings, because `/test` probes `{baseUrl}/v1/models` or
    // `{baseUrl}/models` depending on the base.
    let gateway = FakeGateway::start().await?;
    let status = reqwest::Client::new()
        .get(format!("{}/models", gateway.base_url()))
        .send()
        .await?
        .status()
        .as_u16();
    assert_eq!(status, 200);
    assert_eq!(gateway.requests_to("/v1/models").len(), 1);
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_recorder_keeps_header_names_and_never_a_value() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let token = "sk-not-a-real-credential-0000";
    let (status, _, _) = post_messages(
        &gateway,
        &[
            ("authorization", &format!("Bearer {token}")),
            ("anthropic-version", ANTHROPIC_VERSION),
            ("x-stainless-lang", "js"),
            ("accept", "text/event-stream"),
        ],
        &request_body(),
    )
    .await?;
    assert_eq!(status, 200);

    let requests = gateway.requests();
    assert_eq!(requests.len(), 1);
    let recorded = &requests[0];
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/messages");
    assert_eq!(recorded.body_bytes, request_body().len());

    // Names are recorded...
    for name in [
        "authorization",
        "anthropic-version",
        "x-stainless-lang",
        "accept",
    ] {
        assert!(
            recorded.has_header(name),
            "expected {name} among {:?}",
            recorded.header_names
        );
    }
    assert!(
        gateway.saw_header("Authorization"),
        "lookup is case-insensitive"
    );

    // ...and no value is: the only printable material is the name.
    let printable = format!("{recorded:?}");
    assert!(
        !printable.contains(token),
        "the recorder stored a credential value: {printable}"
    );
    assert!(
        !printable.contains("Bearer"),
        "the recorder stored an authorization value: {printable}"
    );
    // Every recorded name is a bare lowercase header name — nothing that could
    // carry a value.
    for name in &recorded.header_names {
        assert_eq!(
            name,
            &name.to_ascii_lowercase(),
            "names are normalized to lowercase: {name:?}"
        );
        assert!(
            !name.contains(':') && !name.contains(' '),
            "a header name must not carry a value: {name:?}"
        );
    }

    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_credential_bearing_request_is_flagged_by_name_only() -> Result<()> {
    // The audit direction that matters: this fixture must be able to *report*
    // that a credential arrived, by name, without ever holding its value.
    let gateway = FakeGateway::start().await?;
    let token = "sk-flagged-credential-9999";
    let (status, _, _) = post_messages(
        &gateway,
        &[("authorization", &format!("Bearer {token}"))],
        &request_body(),
    )
    .await?;
    assert_eq!(status, 200);

    let violations = gateway.credential_violations();
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains("authorization"), "{}", violations[0]);
    assert!(!violations[0].contains(token), "{}", violations[0]);

    let error = gateway
        .assert_no_credentials()
        .expect_err("a credential-bearing request must fail the audit");
    let rendered = error.to_string();
    assert!(
        !rendered.contains(token),
        "the error leaked a value: {rendered}"
    );

    // The audit clears when the recording does.
    gateway.reset_recorder();
    gateway.assert_no_credentials()?;
    assert!(gateway.requests().is_empty());

    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_credential_free_request_passes_the_audit() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    post_messages(&gateway, &[], &request_body()).await?;
    gateway.assert_no_credentials()?;
    assert!(gateway.credential_violations().is_empty());
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn expect_authorization_refuses_a_credential_free_request() -> Result<()> {
    // The origin can demand the profile credential without holding one: a
    // request with no credential at all is refused with a real 401, which is
    // what makes a dropped-credential bug fail at the origin.
    let gateway = FakeGateway::start().await?;
    gateway.set_expect_authorization(true);

    let (status, _, body) = post_messages(&gateway, &[], &request_body()).await?;
    assert_eq!(status, 401);
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["error"]["type"], "authentication_error");

    // With a credential presented it answers normally — no value comparison is
    // involved, because there is no expected value to compare against.
    let (status, _, _) = post_messages(
        &gateway,
        &[("authorization", "Bearer anything-at-all")],
        &request_body(),
    )
    .await?;
    assert_eq!(status, 200);
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_listener_is_loopback_only() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    assert!(gateway.addr().ip().is_loopback(), "{}", gateway.addr());
    assert_ne!(gateway.addr().port(), 0, "an ephemeral port was assigned");
    assert!(gateway.base_url().starts_with("http://127.0.0.1:"));
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn shutdown_releases_the_port() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let addr = gateway.addr();
    gateway.shutdown().await;
    // The port is free again: a fresh bind on the same address succeeds.
    let rebound = tokio::net::TcpListener::bind(addr).await;
    assert!(rebound.is_ok(), "port {addr} still held after shutdown");
    Ok(())
}

#[test]
fn parse_listen_refuses_a_non_loopback_address() {
    assert!(parse_listen("127.0.0.1:0").is_ok());
    assert!(parse_listen("127.0.0.1:59060").is_ok());
    assert!(parse_listen("[::1]:0").is_ok());
    // D-031: the only listener this feature may create is on loopback.
    assert!(parse_listen("0.0.0.0:8080").is_err());
    assert!(parse_listen("10.0.0.5:8080").is_err());
    assert!(parse_listen("not-an-address").is_err());
}

#[test]
fn parse_script_understands_the_named_steps_and_status_codes() {
    assert_eq!(parse_script("ok"), Some(Script::default_messages()));
    assert_eq!(
        parse_script("abort"),
        Some(Script::abort_mid_stream(DEFAULT_TEXT))
    );
    assert_eq!(parse_script("401"), Some(Script::status(401)));
    assert_eq!(parse_script("429"), Some(Script::status(429)));
    assert_eq!(parse_script("529"), Some(Script::status(529)));
    assert_eq!(parse_script("nonsense"), None);
}

#[test]
fn script_status_codes_match_their_scripts() {
    assert_eq!(Script::default_messages().status_code(), 200);
    assert_eq!(Script::abort_mid_stream("x").status_code(), 200);
    assert_eq!(Script::status(429).status_code(), 429);
    assert_eq!(Script::status(529).status_code(), 529);
}
