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

/// A settled `turn/result` lifecycle. `status` is `turn_done` or `error`;
/// `result_index > 0` and `queued=0` mark the process's terminal result frame
/// (the driver's own completion heuristic — NOT process-end evidence).
fn turn_result(status: &str, queued: u64, last_error: Option<&str>) -> Value {
    let mut related = serde_json::Map::new();
    related.insert("resultIndex".into(), json!("1"));
    related.insert("numTurns".into(), json!("1"));
    related.insert("queuedTurnCount".into(), json!(queued.to_string()));
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
            "affectsCompletion": true
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
        let task = tokio::spawn(async move {
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
        Self {
            frames: frx,
            appends: atx,
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
    node.append(&id, turn_started());
    let view = ctx
        .wait_until(&id, |v| v["activity"] == json!("working"))
        .await;
    assert_eq!(view["lifecycle"], json!("running"));

    // Settled clean turn result -> idle, lifecycle untouched.
    node.append(&id, turn_result("turn_done", 0, None));
    let view = ctx
        .wait_until(&id, |v| v["activity"] == json!("idle"))
        .await;
    assert_eq!(view["lifecycle"], json!("running"));
    assert!(
        view["lastTurnError"].is_null(),
        "a clean turn leaves no marker"
    );
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

    node.append(&id, turn_started());
    ctx.wait_until(&id, |v| v["activity"] == json!("working"))
        .await;

    // Settled turn ERROR (a 429): turn ended, process alive.
    node.append(
        &id,
        turn_result("error", 0, Some("API Error: 429 no eligible upstream")),
    );
    let view = ctx
        .wait_until(&id, |v| {
            v["activity"] == json!("idle") && v["lastTurnError"].is_object()
        })
        .await;
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
    node.append(&id, turn_started());
    let view = ctx
        .wait_until(&id, |v| v["activity"] == json!("working"))
        .await;
    assert!(
        view["lastTurnError"].is_null(),
        "the next turn start clears the marker"
    );
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
    node.append(&id, turn_started());
    ctx.wait_until(&id, |v| v["activity"] == json!("working"))
        .await;

    // Another prompt is already queued: the result is not settled -> no idle,
    // no marker, still working.
    node.append(&id, turn_result("error", 1, Some("API Error: 429")));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let view = ctx.get_instance(&id).await;
    assert_eq!(view["activity"], json!("working"));
    assert!(view["lastTurnError"].is_null());

    // A Workflow intermediate result (result_index 0) likewise changes nothing.
    let mut intermediate = turn_result("turn_done", 0, None);
    intermediate["payload"]["affectsCompletion"] = json!(false);
    node.append(&id, intermediate);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let view = ctx.get_instance(&id).await;
    assert_eq!(view["activity"], json!("working"));
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
    node.append(&printed, turn_result("error", 0, Some("API Error: 500")));
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
    Ok(())
}
