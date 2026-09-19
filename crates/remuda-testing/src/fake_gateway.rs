//! Offline Anthropic-Messages gateway double.
//!
//! `fake_gateway` is the shared fixture the api-routing tests run against: the
//! Hub-host in-process egress, the proxy host's relay egress, and the worker's
//! listener all need *some* HTTP origin that answers `/v1/messages` with real
//! SSE, so none of them has to reach a live model to prove it worked.
//!
//! It stays strictly offline and deterministic:
//!
//! * `127.0.0.1:0` only — an ephemeral loopback port, never a fixed one, never
//!   a routable address, and no DNS.
//! * The response plan is a [`Script`] list replayed one entry per request; the
//!   final entry repeats, so a test can script one `429` followed by normal
//!   traffic. [`FakeGateway::start`] answers every request with a small SSE
//!   completion.
//! * Only header **names** are recorded — no header value, no body byte, and no
//!   query value is either stored or printable. A test can therefore assert
//!   that the credential swap happened, and that a worker-side request never
//!   carried the gateway credential, without a real token existing anywhere.
//!   [`FakeGateway::assert_no_credentials`] is the explicit audit for the
//!   direction that matters: nothing credential-bearing ever reached this
//!   origin.
//!
//! ## What this double cannot do
//!
//! It holds no credential of its own, so it cannot compare a presented bearer
//! against an expected secret — there is no secret to compare against, by
//! design, because a fixture that contained one would leak it into every test
//! that printed a failure. Two honest substitutes are provided instead:
//!
//! * [`FakeGateway::set_expect_authorization`] makes the server refuse, with a
//!   real `401 authentication_error`, any request that presents no credential
//!   at all. That covers the "the relay forgot to attach the profile
//!   credential" failure, which is the one this fixture is asked to catch.
//! * [`Script::Status`] scripts a `401`/`403` for the value-mismatch case, so a
//!   relay that forwards a *wrong* credential can still be tested end to end.
//!
//! Responses are scripted rather than modeled: the SSE event sequence is the
//! documented Anthropic shape (`message_start`, `content_block_start`,
//! `content_block_delta` × n, `content_block_stop`, `message_delta`,
//! `message_stop`), but the token stream is a deterministic split of the
//! scripted text, not generated content.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use futures::stream::StreamExt as _;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The media type the Anthropic streaming endpoint answers with.
pub const SSE_CONTENT_TYPE: &str = "text/event-stream";

/// Default model id echoed by `/v1/messages` and listed by `/v1/models`.
///
/// Deliberately fictional: a fixture naming a real model would invite a test to
/// depend on that model's behaviour.
pub const DEFAULT_MODEL: &str = "fake/model-1";

/// The `anthropic-version` value a real client sends.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The text [`Script::default_messages`] streams back.
pub const DEFAULT_TEXT: &str = "Hello from the fake gateway.";

/// Largest request body the recorder will read.
///
/// A relay test may push a multi-megabyte body to prove chunking works; the
/// recorder needs only a byte count, so the cap is generous but finite.
const MAX_AUDITED_BODY: usize = 16 * 1024 * 1024;

/// Header names that carry a credential. Already lowercase.
///
/// Presenting one of these to the worker-side origin is exactly the leak the
/// api-routing design forbids: the worker holds a per-instance relay bearer,
/// never the gateway credential (D-047 §Failure behaviour, §Secrets).
const CREDENTIAL_HEADER_NAMES: &[&str] = &[
    "authorization",
    "x-api-key",
    "api-key",
    "apikey",
    "proxy-authorization",
    "x-anthropic-api-key",
];

/// Header-name prefixes that carry a credential or a signed identity.
const CREDENTIAL_HEADER_PREFIXES: &[&str] = &["x-stainless-", "x-amz-"];

/// Query-parameter names that would carry a credential inside the URL.
const CREDENTIAL_QUERY_NAMES: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "token",
    "auth",
    "access_token",
    "signature",
];

/// Top-level JSON body keys that would carry a credential in the payload.
const CREDENTIAL_BODY_KEYS: &[&str] = &[
    "apikey",
    "api_key",
    "auth_token",
    "authtoken",
    "token",
    "secret",
    "password",
    "credential",
    "credentials",
];

/// Failures while starting or running the fake gateway.
#[derive(Debug, Error)]
pub enum FakeGatewayError {
    /// The loopback listener could not be bound.
    #[error("bind: {0}")]
    Io(#[from] std::io::Error),
    /// The listen address was not loopback, or not `host:port`.
    #[error("listen address: {0}")]
    Listen(String),
    /// A scripted response could not be encoded.
    #[error("encode: {0}")]
    Json(#[from] serde_json::Error),
    /// Credential-bearing material reached the gateway; see
    /// [`FakeGateway::assert_no_credentials`].
    #[error("credential reached the fake gateway: {}", .0.join("; "))]
    CredentialLeak(Vec<String>),
}

/// What the gateway does with the next request.
///
/// A list of these is replayed in order, one per request, and the final entry
/// repeats forever — so a test that wants one `429` before normal traffic
/// writes `vec![Script::status(429), Script::default_messages()]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Script {
    /// A complete SSE `/v1/messages` completion carrying `text`.
    Messages {
        /// Text streamed as `text_delta` events.
        text: String,
    },
    /// A streaming completion whose response head is delayed first.
    ///
    /// The delay happens before the status line, so a consumer's first-byte
    /// timeout (the relay's ladder allows 60 s) is what fires — not a body
    /// read timeout. This is the fixture for the "slow first byte" case.
    SlowFirstByte {
        /// How long to hold the request before answering at all.
        delay: Duration,
        /// Text streamed once the head is finally sent.
        text: String,
    },
    /// A JSON (non-streaming) `/v1/messages` response carrying `text`.
    MessagesJson {
        /// Text of the single `text` content block.
        text: String,
    },
    /// A bare status with an Anthropic-shaped error body and no messages.
    Status {
        /// HTTP status code (`429`, `529`, `401`, …).
        code: u16,
        /// The `error.type` string.
        error_type: String,
        /// The `error.message` string.
        message: String,
        /// Seconds advertised in `retry-after`; the header is omitted at zero.
        retry_after_secs: u64,
    },
    /// Write the SSE head and some deltas, then drop the connection.
    ///
    /// The response is chunked, ends without the terminal `message_stop`, and
    /// the connection is aborted mid-body — so a consumer observes a genuinely
    /// truncated stream rather than a clean close.
    AbortMidStream {
        /// Text whose leading deltas are written before the connection dies.
        text: String,
    },
}

impl Script {
    /// An SSE completion of `text` under [`DEFAULT_MODEL`].
    pub fn messages(text: impl Into<String>) -> Self {
        Self::Messages { text: text.into() }
    }

    /// An SSE completion of [`DEFAULT_TEXT`] under [`DEFAULT_MODEL`].
    pub fn default_messages() -> Self {
        Self::messages(DEFAULT_TEXT)
    }

    /// A completion whose response head is held for `delay` first.
    pub fn slow_first_byte(delay: Duration) -> Self {
        Self::SlowFirstByte {
            delay,
            text: DEFAULT_TEXT.to_string(),
        }
    }

    /// A non-streaming JSON completion of `text`.
    pub fn messages_json(text: impl Into<String>) -> Self {
        Self::MessagesJson { text: text.into() }
    }

    /// A bare status using the canonical error type for that code.
    ///
    /// `429` → `rate_limit_error` with a `retry-after` of one second, `529` →
    /// `overloaded_error`, `401` → `authentication_error`. Any other code keeps
    /// its canonical type when one is known and `api_error` otherwise.
    pub fn status(code: u16) -> Self {
        Self::Status {
            code,
            error_type: canonical_error_type(code)
                .unwrap_or("api_error")
                .to_string(),
            message: format!("fake gateway: scripted {code}"),
            retry_after_secs: u64::from(code == 429),
        }
    }

    /// A status with an explicit `error.type` and message.
    pub fn status_with(
        code: u16,
        error_type: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Status {
            code,
            error_type: error_type.into(),
            message: message.into(),
            retry_after_secs: u64::from(code == 429),
        }
    }

    /// An SSE head plus leading deltas, then a dropped connection.
    pub fn abort_mid_stream(text: impl Into<String>) -> Self {
        Self::AbortMidStream { text: text.into() }
    }

    /// The HTTP status this script answers with.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Messages { .. } | Self::SlowFirstByte { .. } | Self::MessagesJson { .. } => 200,
            Self::Status { code, .. } => *code,
            Self::AbortMidStream { .. } => 200,
        }
    }
}

/// Canonical Anthropic `error.type` for a status code.
fn canonical_error_type(code: u16) -> Option<&'static str> {
    Some(match code {
        400 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500 => "api_error",
        529 => "overloaded_error",
        _ => return None,
    })
}

/// One request the gateway received, with header **names** only.
///
/// No header value, no body byte and no query value is stored: the recorder
/// exists so a test can assert *which* headers were presented — did the relay
/// replace the worker's bearer with the profile credential, did it keep
/// `anthropic-version`, did it drop `set-cookie` — without a real credential
/// existing in the test process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// HTTP method, uppercase.
    pub method: String,
    /// Request path, without the query string.
    pub path: String,
    /// Lowercased header names, in the order received.
    pub header_names: Vec<String>,
    /// Request body length in bytes (a count, never content).
    pub body_bytes: usize,
}

impl RecordedRequest {
    /// Whether a header with this name was present; case-insensitive.
    pub fn has_header(&self, name: &str) -> bool {
        self.header_names.contains(&name.to_ascii_lowercase())
    }
}

/// Shared, restartable HTTP origin for the api-routing tests.
///
/// See the module docs for the guarantees. The handle owns the accept loop and
/// stops it on `Drop`, so a test that never calls [`FakeGateway::shutdown`]
/// still cannot leak a listener for the rest of the run.
pub struct FakeGateway {
    addr: SocketAddr,
    inner: Arc<GatewayInner>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// The state each handler shares.
struct GatewayInner {
    /// Response plan, replayed one entry per request; the last entry repeats.
    scripts: Vec<Script>,
    /// Refuse a request that presents no credential at all.
    expect_authorization: AtomicBool,
    /// Recording, behind one lock so a handler and a test never interleave.
    recorded: Mutex<Recorded>,
}

#[derive(Default)]
struct Recorded {
    /// Requests answered so far; indexes into `GatewayInner::scripts`, clamped.
    served: usize,
    /// One row per answered request, oldest first.
    requests: Vec<RecordedRequest>,
    /// Credential-bearing material seen, described by key name only.
    violations: Vec<String>,
}

impl FakeGateway {
    /// Bind an ephemeral loopback port and answer every request with
    /// [`Script::default_messages`].
    pub async fn start() -> Result<Self, FakeGatewayError> {
        Self::start_with(vec![Script::default_messages()]).await
    }

    /// Bind an ephemeral loopback port and replay `scripts` in order.
    pub async fn start_with(scripts: Vec<Script>) -> Result<Self, FakeGatewayError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        Self::serve(listener, scripts).await
    }

    /// Serve on an already-bound listener.
    ///
    /// The caller owns the bind, which is what lets the `fake-gateway` binary
    /// honour `FAKE_GATEWAY_LISTEN` — including port `0` — and print the
    /// address it actually got.
    pub async fn serve(
        listener: TcpListener,
        scripts: Vec<Script>,
    ) -> Result<Self, FakeGatewayError> {
        let addr = listener.local_addr()?;
        let scripts = if scripts.is_empty() {
            vec![Script::default_messages()]
        } else {
            scripts
        };
        let inner = Arc::new(GatewayInner {
            scripts,
            expect_authorization: AtomicBool::new(false),
            recorded: Mutex::new(Recorded::default()),
        });
        let router = Router::new()
            .route("/v1/messages", post(messages))
            .route("/v1/models", get(models))
            .route("/v1/models/{id}", get(models))
            .route("/models", get(models))
            .with_state(inner.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self {
            addr,
            inner,
            shutdown: Some(tx),
            task: Some(task),
        })
    }

    /// The bound loopback address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The origin to hand a profile's `baseUrl`, with no trailing slash.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The origin plus `/v1`, the shape a gateway base URL usually takes.
    pub fn base_url_v1(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Refuse, with a real `401`, any request that presents no credential.
    ///
    /// Off by default: most tests want to send a fully credential-free request
    /// and assert that nothing credential-bearing arrived. A relay test that
    /// needs the origin to *demand* the profile credential turns it on, so a
    /// dropped-credential bug fails loudly at the origin instead of silently
    /// succeeding.
    pub fn set_expect_authorization(&self, expect: bool) {
        self.inner
            .expect_authorization
            .store(expect, Ordering::SeqCst);
    }

    /// Requests answered so far, oldest first.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().requests.clone()
    }

    /// How many requests have been answered.
    pub fn request_count(&self) -> usize {
        self.lock().requests.len()
    }

    /// Requests answered for one exact path.
    pub fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        self.lock()
            .requests
            .iter()
            .filter(|request| request.path == path)
            .cloned()
            .collect()
    }

    /// Every distinct header name seen, in first-seen order.
    pub fn header_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for request in &self.lock().requests {
            for name in &request.header_names {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    /// Whether any request presented this header; case-insensitive.
    pub fn saw_header(&self, name: &str) -> bool {
        self.lock().requests.iter().any(|r| r.has_header(name))
    }

    /// Forget every recorded request and violation, keeping the script cursor.
    pub fn reset_recorder(&self) {
        let mut recorded = self.lock();
        recorded.requests.clear();
        recorded.violations.clear();
    }

    /// Credential-bearing material seen so far, described by key name only.
    ///
    /// Never returns a value — none is stored.
    pub fn credential_violations(&self) -> Vec<String> {
        self.lock().violations.clone()
    }

    /// Fail if any request carried a credential; otherwise succeed.
    ///
    /// This is the audit for the direction the design cares about: a worker-side
    /// request — and anything derived from one — must reach the gateway with the
    /// profile credential attached and the *worker's* credential absent. It
    /// takes the gateway's own recorded material as its evidence, so a failure
    /// message can name the offending header without quoting it.
    pub fn assert_no_credentials(&self) -> Result<(), FakeGatewayError> {
        let violations = self.credential_violations();
        if violations.is_empty() {
            Ok(())
        } else {
            Err(FakeGatewayError::CredentialLeak(violations))
        }
    }

    /// Stop serving and release the port.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Recorded> {
        self.inner
            .recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl GatewayInner {
    fn lock(&self) -> MutexGuard<'_, Recorded> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record one answered request and any credential-bearing names it carried.
    fn record(&self, request: RecordedRequest, query_names: &[String], body_keys: &[String]) {
        let mut violations = Vec::new();
        for name in &request.header_names {
            if is_credential_header(name) {
                violations.push(format!(
                    "request carried credential-bearing header {name:?}"
                ));
            }
        }
        for name in query_names {
            if CREDENTIAL_QUERY_NAMES.contains(&name.as_str()) {
                violations.push(format!(
                    "request carried credential-bearing query parameter {name:?}"
                ));
            }
        }
        for key in body_keys {
            if CREDENTIAL_BODY_KEYS.contains(&key.as_str()) {
                violations.push(format!(
                    "request body carried credential-bearing key {key:?}"
                ));
            }
        }
        let mut recorded = self.lock();
        recorded.requests.push(request);
        recorded.violations.extend(violations);
    }

    /// Take the script for the next request and advance the cursor.
    fn next_script(&self) -> Script {
        let mut recorded = self.lock();
        let index = recorded.served.min(self.scripts.len().saturating_sub(1));
        recorded.served += 1;
        self.scripts[index].clone()
    }

    /// Whether the request presented any credential at all.
    fn presents_credential(header_names: &[String]) -> bool {
        header_names
            .iter()
            .any(|name| CREDENTIAL_HEADER_NAMES.contains(&name.as_str()))
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// `/v1/messages` — the scripted responder.
///
/// The request body is read to completion (so a chunked upload is fully
/// consumed and its length recorded) but never interpreted: only its top-level
/// *keys* are inspected, and only to assert that no credential key is present.
async fn messages(State(gateway): State<Arc<GatewayInner>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, MAX_AUDITED_BODY).await {
        Ok(body) => body,
        Err(_) => return error_body(413, "request_too_large", "fake gateway: body too large", 0),
    };
    let header_names: Vec<String> = parts
        .headers
        .keys()
        .map(|name| name.as_str().to_ascii_lowercase())
        .collect();
    let query_names = query_names(parts.uri.query().unwrap_or(""));
    let body_keys = body_keys(&body);
    let recorded = RecordedRequest {
        method: parts.method.as_str().to_ascii_uppercase(),
        path: parts.uri.path().to_string(),
        header_names: header_names.clone(),
        body_bytes: body.len(),
    };
    gateway.record(recorded, &query_names, &body_keys);

    if gateway.expect_authorization.load(Ordering::SeqCst)
        && !GatewayInner::presents_credential(&header_names)
    {
        return error_body(
            401,
            "authentication_error",
            "fake gateway: no credential presented",
            0,
        );
    }

    match gateway.next_script() {
        Script::Messages { text } => sse_response(&text, false),
        Script::AbortMidStream { text } => sse_response(&text, true),
        Script::SlowFirstByte { delay, text } => {
            tokio::time::sleep(delay).await;
            sse_response(&text, false)
        }
        Script::MessagesJson { text } => json_response(
            200,
            &json!({
                "id": "msg_fake",
                "type": "message",
                "role": "assistant",
                "model": DEFAULT_MODEL,
                "content": [{ "type": "text", "text": text }],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": { "input_tokens": 8, "output_tokens": token_count(&text) },
            }),
        ),
        Script::Status {
            code,
            error_type,
            message,
            retry_after_secs,
        } => error_body(code, &error_type, &message, retry_after_secs),
    }
}

/// `/v1/models` — the two-listings-per-gateway behaviour.
///
/// One observed gateway answers a plain `Authorization: Bearer` GET with its
/// broad OpenAI-style catalog, but only the short `claude-*` subset once
/// `anthropic-version` is set (`docs/design/providers.md` §Two listings per
/// gateway). The fake reproduces that exactly, so a discovery probe that reads
/// one surface under-reports in a test the same way it does against the real
/// thing.
///
/// One id (`claude-fake-shared`) appears on both surfaces, which is what makes
/// the union non-trivial, and one (`fake/model-1m`) reports a context window at
/// or above `LONG_CONTEXT_TOKENS`, which is what earns the `1m` tag.
async fn models(State(gateway): State<Arc<GatewayInner>>, headers: HeaderMap) -> Response {
    // Recorded like `messages` is, so a test can assert what a discovery probe
    // sent (which surfaces it read, whether it presented a credential) without
    // the fixture holding one.
    let header_names: Vec<String> = headers
        .keys()
        .map(|name| name.as_str().to_ascii_lowercase())
        .collect();
    gateway.record(
        RecordedRequest {
            method: "GET".to_string(),
            path: "/v1/models".to_string(),
            header_names,
            body_bytes: 0,
        },
        &[],
        &[],
    );
    let body = if headers.contains_key("anthropic-version") {
        json!({
            "data": [
                { "type": "model", "id": DEFAULT_MODEL, "display_name": "Fake Model 1",
                  "context_window": 200_000 },
                { "type": "model", "id": "fake/model-1m", "display_name": "Fake Model 1M",
                  "context_window": 1_048_576 },
                { "type": "model", "id": "claude-fake-shared", "display_name": "Claude Fake" }
            ],
            "has_more": false,
        })
    } else {
        json!({
            "object": "list",
            "data": [
                { "id": DEFAULT_MODEL, "object": "model", "context_length": 200_000 },
                { "id": "fake/model-1m", "object": "model", "context_length": 1_048_576 },
                { "id": "passthrough/fake-evolving", "object": "model" },
                { "id": "claude-fake-shared", "object": "model" }
            ],
        })
    };
    json_response(200, &body)
}

fn json_response(code: u16, body: &Value) -> Response {
    encode_response(code, "application/json", body.to_string(), None)
}

fn error_body(code: u16, error_type: &str, message: &str, retry_after_secs: u64) -> Response {
    let body = json!({
        "type": "error",
        "error": { "type": error_type, "message": message },
        "request_id": "req_fake",
    })
    .to_string();
    encode_response(
        code,
        "application/json",
        body,
        Some(retry_after_secs).filter(|secs| *secs > 0),
    )
}

fn encode_response(
    code: u16,
    content_type: &str,
    body: String,
    retry_after_secs: Option<u64>,
) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len());
    if let Some(secs) = retry_after_secs {
        builder = builder.header("retry-after", secs.to_string());
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Build the SSE response, optionally truncating it mid-body.
///
/// `abort` writes the head and the leading deltas as a chunked response whose
/// stream then yields an error. Hyper aborts the connection at that point, so
/// the peer sees a truncated SSE stream — no `message_stop` — and not a clean
/// close. That is the difference a mid-stream-abort test has to be able to
/// observe, and it is why the abort body is a stream rather than a short
/// `String`.
fn sse_response(text: &str, abort: bool) -> Response {
    let mut out = String::new();
    push_event(
        &mut out,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_fake",
                "type": "message",
                "role": "assistant",
                "model": DEFAULT_MODEL,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 8, "output_tokens": 0 },
            },
        }),
    );
    push_event(
        &mut out,
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" },
        }),
    );

    let deltas = tokens(text);
    // An abort must still deliver real deltas first, or the peer cannot tell
    // truncation from an immediate close.
    let written = if abort {
        deltas.len().div_ceil(2).clamp(1, deltas.len())
    } else {
        deltas.len()
    };
    for token in deltas.iter().take(written) {
        push_event(
            &mut out,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": token },
            }),
        );
    }
    // A truncated SSE document stops here: the last `data:` line has no
    // terminating blank line, and the stream never reaches `message_stop`.
    let body = if abort {
        out
    } else {
        push_event(
            &mut out,
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": 0 }),
        );
        push_event(
            &mut out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": deltas.len() as u64 },
            }),
        );
        push_event(&mut out, "message_stop", &json!({ "type": "message_stop" }));
        out
    };

    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, SSE_CONTENT_TYPE)
        .header(header::CACHE_CONTROL, "no-cache");
    let builder = if abort {
        // Chunked, because the length is not what the peer should trust.
        builder.header(header::TRANSFER_ENCODING, "chunked")
    } else {
        builder.header(header::CONTENT_LENGTH, body.len())
    };
    let body = if abort {
        // Two frames: the SSE prefix, then — after a short pause so the prefix
        // is actually flushed to the socket — an error that makes hyper reset
        // the connection. Yielding both in one poll would race the flush and
        // the peer would see an empty reply instead of a truncated stream,
        // which is a different failure and would make an abort test prove
        // nothing.
        let prefix =
            futures::stream::once(async move { Ok::<Bytes, std::io::Error>(Bytes::from(body)) });
        let abort = futures::stream::once(async {
            tokio::time::sleep(ABORT_FLUSH_GRACE).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "fake gateway: scripted mid-stream abort",
            ))
        });
        Body::from_stream(prefix.chain(abort))
    } else {
        Body::from(body)
    };
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// How long [`Script::AbortMidStream`] waits before resetting the connection.
///
/// Long enough for hyper to flush the prefix frame it was just handed, short
/// enough that a test never notices. Without it the reset wins the race and the
/// peer observes an empty reply rather than a truncated stream.
const ABORT_FLUSH_GRACE: Duration = Duration::from_millis(50);

fn push_event(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

/// Split text into the pieces the SSE deltas carry.
///
/// Deterministic and lossless: concatenating the result reproduces `text`
/// exactly, so a test can reassemble a streamed body and compare it with the
/// script.
fn tokens(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.chars().count() >= 5 {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn token_count(text: &str) -> u64 {
    tokens(text).len() as u64
}

/// Query parameter **names** only; no value is read.
fn query_names(query: &str) -> Vec<String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            pair.split('=')
                .next()
                .unwrap_or(pair)
                .trim()
                .to_ascii_lowercase()
        })
        .collect()
}

/// Top-level JSON body **keys** only; no value is read.
fn body_keys(body: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    match value.as_object() {
        Some(map) => map.keys().map(|key| key.to_ascii_lowercase()).collect(),
        None => Vec::new(),
    }
}

fn is_credential_header(name: &str) -> bool {
    CREDENTIAL_HEADER_NAMES.contains(&name)
        || CREDENTIAL_HEADER_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// A loopback-only listen address for the `fake-gateway` binary.
///
/// Refuses anything that is not a literal loopback address, so the binary
/// cannot be pointed at a routable interface by accident — D-031 permits no
/// listener other than a loopback one.
pub fn parse_listen(spec: &str) -> Result<SocketAddr, FakeGatewayError> {
    let addr: SocketAddr = spec
        .parse()
        .map_err(|_| FakeGatewayError::Listen(format!("{spec:?} is not host:port")))?;
    if !addr.ip().is_loopback() {
        return Err(FakeGatewayError::Listen(format!("{addr} is not loopback")));
    }
    Ok(addr)
}

/// Parse a `--script` / `FAKE_GATEWAY_SCRIPT` value into a [`Script`].
///
/// Accepts `ok`, `json`, `abort`, and any bare status code (`429`, `529`, …).
pub fn parse_script(spec: &str) -> Option<Script> {
    Some(match spec {
        "ok" | "messages" => Script::default_messages(),
        "json" => Script::messages_json(DEFAULT_TEXT),
        "abort" => Script::abort_mid_stream(DEFAULT_TEXT),
        other => Script::status(other.parse().ok()?),
    })
}

/// Default listen address for the `fake-gateway` binary.
///
/// Port `0` so a test can start several instances without collisions; the
/// binary prints the address it actually got on stdout.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:0";

/// The `fake-gateway` binary's CLI entry.
///
/// Configuration is environment-only, matching the other fakes in this crate:
///
/// * `FAKE_GATEWAY_LISTEN` — a loopback `host:port` (default
///   [`DEFAULT_LISTEN`]); a non-loopback address is refused.
/// * `FAKE_GATEWAY_SCRIPT` — comma-separated [`parse_script`] values, replayed
///   one per request (default `ok`).
///
/// On success it prints `fake gateway: <addr>` to stdout, which is the line a
/// test waits for. It never reads or prints a credential: there is none.
pub fn run_fake_gateway() -> Result<(), FakeGatewayError> {
    let listen =
        std::env::var("FAKE_GATEWAY_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let scripts = std::env::var("FAKE_GATEWAY_SCRIPT")
        .unwrap_or_else(|_| "ok".to_string())
        .split(',')
        .filter(|spec| !spec.trim().is_empty())
        .map(|spec| parse_script(spec.trim()))
        .collect::<Option<Vec<Script>>>()
        .ok_or_else(|| {
            FakeGatewayError::Listen("FAKE_GATEWAY_SCRIPT has an unknown step".into())
        })?;
    let addr = parse_listen(&listen)?;
    let parent_watch = crate::parent_watch::install();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = TcpListener::bind(addr).await?;
        let gateway = FakeGateway::serve(listener, scripts).await?;
        // The readiness line: a test greps stdout for it rather than polling
        // the port, so a bind failure is reported as a bind failure.
        println!("fake gateway: {}", gateway.addr());
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        match parent_watch {
            Some(parent_watch) => {
                tokio::select! {
                    biased;
                    _ = parent_watch.exited() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        Ok(())
    })
}
