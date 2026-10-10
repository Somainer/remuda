//! D-057 ma-sdk-state: claude-sdk / claude-print turn working/idle projection.
//!
//! Fixture replay through the real Hub journal fold:
//! * a user frame (`turn/turn_started`) gives activity `working`;
//! * a settled `turn/result` (`turn_done`) gives `idle`;
//! * a settled result `error` (an API error such as 429) leaves lifecycle
//!   `running`, activity `idle`, and sets an additive `lastTurnError` marker;
//! * the next turn start clears the marker;
//! * an instance holding `address-owner` stays an active holder after a turn
//!   error (a second address-owner insert still gets 409);
//! * genuine start-failure evidence still projects `failed`.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8600-8600000000ee";
const TIMEOUT: Duration = Duration::from_secs(8);

/// A native `session/started` observation the driver emits once initialized.
fn session_started(session: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": "session",
            "nativeId": { "state": "known", "value": session },
            "status": { "state": "known", "value": "started" },
            "relatedIds": {},
            "dataRef": null,
            "severity": "info",
            "affectsCompletion": false
        }
    })
}

/// A `turn/turn_started` lifecycle the engine emits after the user frame is
/// written to the native process.
fn turn_started() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "turn",
            "nativeName": "turn_started",
            "nativeId": { "state": "known", "value": SESSION },
            "status": { "state": "known", "value": "working" },
            "relatedIds": { "nativeClientMessageId": "msg-1" },
            "dataRef": null,
            "severity": "info",
            "affectsCompletion": false
        }
    })
}

/// A `turn/result` lifecycle shaped EXACTLY like the driver's `map_result`.
///
/// `index` is the native `result_index`, `queued` the `queued_turn_count`,
/// `settled_root_turn` the driver's explicit root-turn decision
/// (`relatedIds.settledRootTurn`), and `affects_completion` the independent
/// one-shot print-process heuristic (`index > 0 && queued == 0`). The first
/// turn of a long-lived sdk session is `index=0, queued=0` with
/// `affects_completion=false` but `settled_root_turn=true`.
fn turn_result(
    status: &str,
    index: u64,
    queued: u64,
    settled_root_turn: bool,
    last_error: Option<&str>,
) -> Value {
    let mut related = serde_json::Map::new();
    related.insert("resultIndex".into(), json!(index.to_string()));
    related.insert("numTurns".into(), json!("1"));
    related.insert("queuedTurnCount".into(), json!(queued.to_string()));
    if settled_root_turn {
        related.insert("settledRootTurn".into(), json!("true"));
    }
    if let Some(text) = last_error {
        related.insert("lastError".into(), json!(text));
    }
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "turn",
            "nativeName": "result",
            "nativeId": { "state": "known", "value": SESSION },
            "status": { "state": "known", "value": status },
            "relatedIds": related,
            "dataRef": null,
            "severity": "info",
            // The one-shot print heuristic — deliberately distinct from
            // settledRootTurn.
            "affectsCompletion": index > 0 && queued == 0
        }
    })
}

/// A settled FIRST turn of a long-lived sdk/print session: index 0, queued 0,
/// print heuristic false, but the driver stamps settledRootTurn=true.
fn first_turn_result(status: &str, last_error: Option<&str>) -> Value {
    turn_result(status, 0, 0, true, last_error)
}

/// A `system/task_started` (local_workflow) opening a background workflow, so
/// its following index-0 result is an intermediate that must not idle.
fn workflow_started() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "task",
            "nativeName": "task_started",
            "nativeId": { "state": "known", "value": "wf-1" },
            "status": { "state": "known", "value": "running" },
            "relatedIds": {},
            "dataRef": null,
            "severity": "info",
            "affectsCompletion": false
        }
    })
}

/// Real start-failure evidence: the launch never started.
fn start_failed() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": "native-driver-start-failed",
            "nativeId": { "state": "known", "value": "not-applicable" },
            "status": { "state": "known", "value": "failed" },
            "relatedIds": {
                "reasonCode": "native-driver-start-failed",
                "lastError": "agent process exited during startup"
            },
            "dataRef": null,
            "severity": "error",
            "affectsCompletion": true
        }
    })
}

/// A real process exit (the separate topic=session event the driver emits when
/// a one-shot child actually ends).
fn session_exited() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" }
    })
}

struct FakeNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    /// Captured panic from the background fake-Node loop (r2 item 6).
    task_error: Arc<tokio::sync::Mutex<Option<String>>>,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    async fn connect(hub: &remuda_hub::RunningHub, host: &str) -> Self {
        let enroll = hub.mint_enroll_token(5).await.unwrap();
        let mut req = format!("ws://{}/v1/node", hub.addr)
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws.send(Message::Text(
            json!({
                "jsonrpc":"2.0","id":"hello","method":"node.hello",
                "params": { "hostId": host, "host": {
                    "hostname":"lineage-node","workspaces":[], "workspaceRevision":0,
                    "herdr":{"path":"/usr/bin/herdr"},
                    "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged-in"}]
                }}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let hello = match ws.next().await {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(&t).unwrap(),
            other => panic!("hello: {other:?}"),
        };
        assert!(hello.get("result").is_some(), "hello failed: {hello}");

        let (ftx, frx) = tokio::sync::mpsc::unbounded_channel();
        let (atx, mut arx) = tokio::sync::mpsc::unbounded_channel();
        // r2 item 6: capture a panic from the fake-Node loop so a test fails
        // loudly instead of the background task dying silently.
        let task_error: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let task_error_for_task = task_error.clone();
        let task = tokio::spawn(async move {
            let body = std::panic::AssertUnwindSafe(async {
                loop {
                    tokio::select! {
                        Some((id, event)) = arx.recv() => {
                            ws.send(Message::Text(json!({
                                "jsonrpc":"2.0","id":"append","method":"journal.append",
                                "params":{"instanceId":id,"event":event}
                            }).to_string().into())).await.unwrap();
                            match ws.next().await {
                                Some(Ok(Message::Text(t))) => {
                                    let f: Value = serde_json::from_str(&t).unwrap();
                                    assert_eq!(f["id"], json!("append"));
                                    assert!(f.get("result").is_some());
                                }
                                _ => break,
                            }
                        }
                        frame = ws.next() => {
                            match frame {
                                Some(Ok(Message::Text(text))) => {
                                    let f: Value = serde_json::from_str(&text).unwrap();
                                    let Some(method) = f["method"].as_str() else { continue };
                                    if ftx.send((method.to_owned(), f["params"].clone())).is_err() {
                                        break;
                                    }
                                    ws.send(Message::Text(json!({
                                        "jsonrpc":"2.0","id":f["id"],
                                        "result":{"accepted":true}
                                    }).to_string().into())).await.unwrap();
                                }
                                Some(Ok(Message::Close(_))) | None => break,
                                _ => {}
                            }
                        }
                    }
                }
            });
            use futures::FutureExt;
            if body.catch_unwind().await.is_err() {
                *task_error_for_task.lock().await = Some("fake-node task panicked".to_string());
            }
        });
        Self {
            frames: frx,
            appends: atx,
            task_error,
            _task: task,
        }
    }

    async fn next_frame(&mut self) -> (String, Value) {
        tokio::time::timeout(TIMEOUT, self.frames.recv())
            .await
            .context("frame")
            .unwrap()
            .expect("frame channel closed")
    }

    fn append(&self, id: &str, event: Value) {
        self.appends.send((id.to_owned(), event)).unwrap();
    }

    /// Panic captured from the fake-Node background loop, if any.
    async fn task_error(&self) -> Option<String> {
        self.task_error.lock().await.clone()
    }
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self._task.abort();
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    cookie: String,
    host: String,
}

/// Raw HTTP returning status, headers-head and body (for the login cookie).
async fn raw_http_full(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), rest.to_string())
}

impl Ctx {
    async fn boot() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hub = spawn(HubConfig::for_test(dir.path().join("data")))
            .await
            .unwrap();
        let host = HostId::new().as_id().as_str().to_owned();
        // Login -> session cookie used by every subsequent HTTP call.
        let body = json!({
            "bootstrapToken": hub.bootstrap_token,
            "deviceName": "sdk-state-phone"
        })
        .to_string();
        let (status, head, _) =
            raw_http_full(hub.addr, "POST", "/v1/login", &[], Some(&body)).await;
        assert_eq!(status, 200);
        let cookie = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
            .and_then(|l| {
                l.split_once(':')
                    .map(|(_, v)| v.trim().split(';').next().unwrap().trim().to_owned())
            })
            .expect("set-cookie");
        Self {
            _dir: dir,
            hub,
            cookie,
            host,
        }
    }

    /// Raw HTTP authenticated with the session cookie.
    async fn raw_http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let (status, _, rest) = raw_http_full(
            self.hub.addr,
            method,
            path,
            &[("Cookie", self.cookie.as_str())],
            body,
        )
        .await;
        (status, rest)
    }

    /// Create a continuity seat holding `address-owner`; returns its id and
    /// drains the forwarded instance.create frame.
    async fn create_holder(&self, node: &mut FakeNode) -> String {
        let body = json!({
            "hostId": self.host,
            "kind": "claude",
            "driver": "claude-sdk",
            "permissionMode": "manual",
            "grants": ["address-owner"],
            "restart": { "onProcessLoss": true, "maxPerHour": 3 },
            "prompt": "seat brief"
        })
        .to_string();
        let (status, text) = self.raw_http("POST", "/v1/instances", Some(&body)).await;
        assert_eq!(status, 200, "{text}");
        let created: Value = serde_json::from_str(text.trim()).unwrap();
        let id = created["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (method, _) = node.next_frame().await;
        assert_eq!(method, "instance.create");
        id
    }

    async fn get_instance(&self, id: &str) -> Value {
        let (status, body) = self
            .raw_http("GET", &format!("/v1/instances/{id}"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(body.trim()).unwrap()
    }

    /// Current durable journal sequence (r2 item 6: wait on this after an
    /// append instead of sleeping, so a "must NOT change" assertion is only
    /// made once the event has actually been folded).
    async fn durable_seq(&self, id: &str) -> u64 {
        self.get_instance(id).await["durableSeq"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Wait until the instance's durable sequence has advanced past `prev`,
    /// proving one appended event was folded; return the fresh view.
    async fn after_append(&self, id: &str, prev: u64) -> Value {
        self.wait_until(id, |v| {
            v["durableSeq"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|seq| seq > prev)
        })
        .await
    }

    async fn wait_until<F>(&self, id: &str, pred: F) -> Value
    where
        F: Fn(&Value) -> bool,
    {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let v = self.get_instance(id).await;
                if pred(&v) {
                    return v;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn user_frame_working_then_settled_turn_done_idles() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.create_holder(&mut node).await;
    node.append(&id, session_started(SESSION));
    ctx.wait_until(&id, |v| v["lifecycle"] == json!("running"))
        .await;

    // The user frame is written -> the root turn starts -> working.
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, turn_started());
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(view["activity"], json!("working"));
    assert_eq!(view["lifecycle"], json!("running"));

    // The FIRST turn settles (index 0, affectsCompletion=false, but
    // settledRootTurn=true): idle, lifecycle untouched.
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, first_turn_result("turn_done", None));
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(view["activity"], json!("idle"));
    assert_eq!(view["lifecycle"], json!("running"));
    assert!(
        view["lastTurnError"].is_null(),
        "a clean turn leaves no marker"
    );
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}

#[tokio::test]
async fn an_error_result_leaves_running_idle_with_a_marker_and_keeps_the_seat_grant() -> Result<()>
{
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.create_holder(&mut node).await;
    node.append(&id, session_started(SESSION));
    ctx.wait_until(&id, |v| v["lifecycle"] == json!("running"))
        .await;

    let seq = ctx.durable_seq(&id).await;
    node.append(&id, turn_started());
    ctx.after_append(&id, seq).await;
    ctx.wait_until(&id, |v| v["activity"] == json!("working"))
        .await;

    // Settled FIRST-turn ERROR (a 429): turn ended, process alive.
    let seq = ctx.durable_seq(&id).await;
    node.append(
        &id,
        first_turn_result("error", Some("API Error: 429 no eligible upstream")),
    );
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(
        view["lifecycle"],
        json!("running"),
        "turn error is never process failure"
    );
    assert_eq!(
        view["activity"],
        json!("idle"),
        "a failed turn still frees the composer"
    );
    let marker = &view["lastTurnError"];
    assert_eq!(marker["text"], json!("API Error: 429 no eligible upstream"));
    assert!(marker["at"].is_string());

    // The seat grant is still held: a second address-owner insert is rejected.
    let body = json!({
        "hostId": ctx.host,
        "kind": "claude", "driver": "claude-pty",
        "grants": ["address-owner"],
        "prompt": "second seat"
    })
    .to_string();
    let (status, text) = ctx.raw_http("POST", "/v1/instances", Some(&body)).await;
    assert_eq!(status, 409, "{text}");
    assert!(text.contains("address-owner"));

    // The next turn start clears the marker and flips activity back to working.
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, turn_started());
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(view["activity"], json!("working"));
    assert!(
        view["lastTurnError"].is_null(),
        "the next turn start clears the marker"
    );
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}

#[tokio::test]
async fn a_queued_or_intermediate_result_does_not_idle_or_mark() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.create_holder(&mut node).await;
    node.append(&id, session_started(SESSION));
    ctx.wait_until(&id, |v| v["lifecycle"] == json!("running"))
        .await;
    let seq0 = ctx.durable_seq(&id).await;
    node.append(&id, turn_started());
    ctx.after_append(&id, seq0).await;
    ctx.wait_until(&id, |v| v["activity"] == json!("working"))
        .await;

    // Another prompt is already queued: the error result is not settled -> no
    // idle, no marker, still working.
    let seq = ctx.durable_seq(&id).await;
    node.append(
        &id,
        turn_result("error", 0, 1, false, Some("API Error: 429")),
    );
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(view["activity"], json!("working"));
    assert!(view["lastTurnError"].is_null());

    // A background workflow opens; its first result (index 0, queued 0) is an
    // INTERMEDIATE result with no settledRootTurn flag — it must not idle.
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, workflow_started());
    ctx.after_append(&id, seq).await;
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, turn_result("turn_done", 0, 0, false, None));
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(view["activity"], json!("working"));
    assert!(view["lastTurnError"].is_null());

    // An intermediate ERROR result (r2 item 5): same settlement decision, error
    // or success, does not settle the root while the workflow is open.
    let seq = ctx.durable_seq(&id).await;
    node.append(
        &id,
        turn_result("error", 0, 0, false, Some("workflow boom")),
    );
    let view = ctx.after_append(&id, seq).await;
    assert_eq!(
        view["activity"],
        json!("working"),
        "an intermediate workflow error does not settle the root"
    );
    assert!(
        view["lastTurnError"].is_null(),
        "an intermediate workflow error sets no marker"
    );
    assert_ne!(view["lifecycle"], json!("failed"));
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}

#[tokio::test]
async fn a_genuine_start_failure_still_projects_failed_and_a_print_turn_error_then_exit_is_exited()
-> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;

    // (a) Start failure (no session ever) -> failed.
    let rejected = ctx.create_holder(&mut node).await;
    node.append(&rejected, start_failed());
    let view = ctx
        .wait_until(&rejected, |v| v["lifecycle"] == json!("failed"))
        .await;
    assert_ne!(
        view["activity"],
        json!("idle"),
        "a start failure is not an idle turn"
    );

    // (b) A settled turn error leaves the chapter running; when the one-shot
    // process then actually exits (separate session event), it ends `exited`
    // — never `failed` from the turn error.
    let printed = ctx.create_holder(&mut node).await;
    node.append(&printed, session_started(SESSION));
    ctx.wait_until(&printed, |v| v["lifecycle"] == json!("running"))
        .await;
    node.append(&printed, first_turn_result("error", Some("API Error: 500")));
    ctx.wait_until(&printed, |v| v["lastTurnError"].is_object())
        .await;
    let mid = ctx.get_instance(&printed).await;
    assert_eq!(
        mid["lifecycle"],
        json!("running"),
        "turn error alone is not terminal"
    );
    node.append(&printed, session_exited());
    let end = ctx
        .wait_until(&printed, |v| v["lifecycle"] == json!("exited"))
        .await;
    assert_eq!(
        end["lifecycle"],
        json!("exited"),
        "the process exit ends it, not failed"
    );
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}

/// r2 item 4: scoped and non-terminal failure evidence never reaches the
/// generic terminal fold. A subagent's turn error, a failed configure switch,
/// and a severity-error diagnostic all leave the ROOT instance running with no
/// idle/marker — they are recorded in their own scope only.
#[tokio::test]
async fn scoped_and_nonterminal_failures_never_fail_the_root_instance() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.create_holder(&mut node).await;
    node.append(&id, session_started(SESSION));
    ctx.wait_until(&id, |v| v["lifecycle"] == json!("running"))
        .await;
    let baseline = ctx.durable_seq(&id).await;

    let mut cases: Vec<(&str, Value)> = Vec::new();
    // Subagent's turn result error (agentId present): its own row.
    let mut sub = first_turn_result("error", Some("subagent boom"));
    sub["payload"]["relatedIds"]["agentId"] = json!("a1");
    cases.push(("subagent-turn-error", sub));
    // Failed live configure switch.
    cases.push((
        "configure-error",
        json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"configuration","nativeName":"instance.configure",
            "status":{"state":"known","value":"error"},"severity":"error",
            "relatedIds":{},"dataRef":null,"affectsCompletion":false}}),
    ));
    // Severity-error diagnostic.
    cases.push((
        "error-diagnostic",
        json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"diagnostic","nativeName":"api_retry",
            "status":{"state":"known","value":"error"},"severity":"error",
            "relatedIds":{},"dataRef":null,"affectsCompletion":false}}),
    ));
    // Subagent session-severity error.
    cases.push((
        "subagent-session-error",
        json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"session","nativeName":"error",
            "status":{"state":"known","value":"error"},"severity":"error",
            "relatedIds":{"agentId":"a1"},"dataRef":null,"affectsCompletion":true}}),
    ));

    let mut seq = baseline;
    for (label, event) in cases {
        node.append(&id, event);
        let view = ctx.after_append(&id, seq).await;
        seq = view["durableSeq"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(seq);
        assert_ne!(
            view["lifecycle"],
            json!("failed"),
            "{label} must not fail the root"
        );
        assert_ne!(
            view["lifecycle"],
            json!("exited"),
            "{label} must not exit the root"
        );
        assert!(
            view["lastTurnError"].is_null(),
            "{label} must not set the root turn-error marker"
        );
    }
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}

/// r3 (items 1-3): replay the REAL recorded fixtures through the shared
/// driver mapper and fold each serialized observation through the Hub's
/// journal state machine (`derive_instance_state` under the store fold). The
/// durable activity after each `result` must match the per-root-turn
/// settlement evidence: an intermediate workflow result leaves the row
/// non-idle, the settled one idles it — regardless of the process-global
/// result_index and including the real stopped-workflow canary whose only
/// result is index 0 AFTER the terminal notification.
#[tokio::test]
async fn recorded_fixtures_fold_to_the_right_activity_per_result() -> Result<()> {
    use remuda_driver::StdoutMapper;
    use remuda_protocol::DriverKind;
    use remuda_testing::fixtures_dir;

    /// Map one fixture into its per-result activity expectations ("idle" when
    /// the mapper stamped settledRootTurn on that result, "working" when it
    /// did not — the row stays non-idle, here asserted as NOT "idle").
    fn mapped_results(relative: &str) -> Vec<Value> {
        let path = fixtures_dir().join(relative);
        let source = std::fs::read_to_string(&path).expect("fixture");
        let mut mapper = StdoutMapper::new(DriverKind::ClaudeSdk, SESSION);
        let mut results = Vec::new();
        for line in source.lines().filter(|line| !line.trim().is_empty()) {
            let value: Value = serde_json::from_str(line).expect("fixture line");
            for observation in mapper.map(value).expect("map frame") {
                let value = serde_json::to_value(&observation).expect("serialize");
                if value.pointer("/payload/nativeName") == Some(&json!("result")) {
                    results.push(value);
                }
            }
        }
        results
    }

    // (fixture, result indices that START a new root turn, expected activity
    // after EACH result). The live driver emits turn_started(working) at every
    // send; replay only carries results, so the boundaries are inserted here
    // exactly as they would occur on the wire.
    let cases: [(&str, &[usize], &[&str]); 5] = [
        ("scripts/ok.jsonl", &[0], &["idle"]),
        ("scripts/twoturn.jsonl", &[0, 1], &["idle", "idle"]),
        (
            "claude/claude-workflow-canary-1.jsonl",
            // The single post-notification result at process-global index 0.
            &[0],
            &["idle"],
        ),
        (
            "scripts/workflow-two.jsonl",
            // One turn: both workflows open; first closed; both terminated.
            &[0],
            &["working", "working", "idle"],
        ),
        (
            "scripts/workflow-later-turn.jsonl",
            // Two turns; the intermediate (nonzero index) belongs to turn 2.
            &[0, 1],
            &["idle", "working", "idle"],
        ),
    ];

    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;

    for (fixture, turn_starts, expected) in cases {
        // Plain instances (no address-owner grant: that grant is one-per-Hub
        // and create_holder is reserved for the seat test).
        let body = json!({
            "hostId": ctx.host,
            "kind": "claude",
            "driver": "claude-sdk",
            "permissionMode": "manual",
            "prompt": "fixture replay"
        })
        .to_string();
        let (status, text) = ctx.raw_http("POST", "/v1/instances", Some(&body)).await;
        assert_eq!(status, 200, "{text}");
        let id = serde_json::from_str::<Value>(text.trim()).unwrap()["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_owned();
        node.next_frame().await; // drain the instance.create forwarded frame
        node.append(&id, session_started(SESSION));
        ctx.wait_until(&id, |v| v["lifecycle"] == json!("running"))
            .await;

        for (result_index, (result_event, expected_activity)) in mapped_results(fixture)
            .into_iter()
            .zip(expected.iter())
            .enumerate()
        {
            // Reproduce the live turn_started(working) at each turn boundary.
            if turn_starts.contains(&result_index) {
                let seq = ctx.durable_seq(&id).await;
                node.append(&id, turn_started());
                ctx.after_append(&id, seq).await;
                ctx.wait_until(&id, |v| v["activity"] == json!("working"))
                    .await;
            }
            let seq = ctx.durable_seq(&id).await;
            node.append(&id, result_event);
            let view = ctx.after_append(&id, seq).await;
            if *expected_activity == "idle" {
                assert_eq!(
                    view["activity"],
                    json!("idle"),
                    "{fixture} result should idle: {view}"
                );
            } else {
                assert_ne!(
                    view["activity"],
                    json!("idle"),
                    "{fixture} intermediate result must keep the root non-idle: {view}"
                );
            }
            // Workflow intermediates never fail the instance.
            assert_ne!(view["lifecycle"], json!("failed"), "{fixture}");
        }
    }
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}

/// ma-sdk-state r5 item 6 (5b): BOTH turns and BOTH results are built with the
/// REAL mapper and folded through the Hub. Turn A's result is mapped while turn
/// B's book is already outstanding (the buffered-result interleave), so only
/// B's result may carry settledRootTurn and idle — A's keeps the row working.
/// No field is hand-set: deleting the mapper's `begin_turn` opens no
/// per-turn book, so A's first result would settle the empty book and idle
/// immediately, which this test fails on.
#[tokio::test]
async fn a_replay_local_root_turn_from_the_real_mapper_projects_working_then_idle() -> Result<()> {
    use remuda_driver::{DriverKind, StdoutMapper};
    use serde_json::json;

    fn sdk_result(index: u64) -> Value {
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": format!("turn {index} done"),
            "stop_reason": "end_turn",
            "session_id": SESSION,
            "result_index": index
        })
    }

    /// The result observation the REAL mapper emits for a native result frame.
    fn mapped_result(mapper: &mut StdoutMapper, index: u64) -> Value {
        mapper
            .map(sdk_result(index))
            .expect("map result")
            .into_iter()
            .find(|o| {
                serde_json::to_value(o)
                    .ok()
                    .and_then(|v| v.pointer("/payload/nativeName").cloned())
                    .as_ref()
                    == Some(&json!("result"))
            })
            .map(|o| serde_json::to_value(&o).expect("serialize"))
            .expect("a result lifecycle observation")
    }

    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.create_holder(&mut node).await;
    node.append(&id, session_started(SESSION));
    ctx.wait_until(&id, |v| v["lifecycle"] == json!("running"))
        .await;

    let mut mapper = StdoutMapper::new(DriverKind::ClaudeSdk, SESSION);
    // Open two root turns up front (A then B), exactly as two sends do.
    mapper.begin_turn();
    mapper.begin_turn();

    // Turn A starts (working) — the real turn_started observation.
    let start_a = mapper
        .turn_started_observation("msg-5b-a")
        .expect("start A");
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, serde_json::to_value(&start_a).expect("serialize"));
    ctx.after_append(&id, seq).await;
    assert_eq!(ctx.get_instance(&id).await["activity"], json!("working"));

    // A's result is mapped while B is still outstanding: it is NOT settled.
    let result_a = mapped_result(&mut mapper, 0);
    assert_eq!(
        result_a.pointer("/payload/relatedIds/settledRootTurn"),
        None,
        "buffered A cannot settle while B is outstanding"
    );
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, result_a);
    ctx.after_append(&id, seq).await;
    assert_eq!(
        ctx.get_instance(&id).await["activity"],
        json!("working"),
        "A's buffered result must not idle the row while B is open"
    );

    // Turn B starts, then its own result settles and idles.
    let start_b = mapper
        .turn_started_observation("msg-5b-b")
        .expect("start B");
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, serde_json::to_value(&start_b).expect("serialize"));
    ctx.after_append(&id, seq).await;

    let result_b = mapped_result(&mut mapper, 1);
    assert_eq!(
        result_b.pointer("/payload/relatedIds/settledRootTurn"),
        Some(&json!("true")),
        "B is the last outstanding turn: its result settles"
    );
    let seq = ctx.durable_seq(&id).await;
    node.append(&id, result_b);
    ctx.wait_until(&id, |v| v["activity"] == json!("idle"))
        .await;
    let view = ctx.get_instance(&id).await;
    assert_eq!(view["activity"], json!("idle"));
    assert_eq!(view["lifecycle"], json!("running"), "process still live");
    assert_eq!(node.task_error().await, None, "fake-node task panicked");
    Ok(())
}
