//! seen-hub (H6): per-owner done-unseen tracking over the real Hub surface.
//!
//! A fake Node feeds observations built by the REAL claude-sdk stdout mapper
//! (`turn_started` at send, the settled `result` at turn end) through
//! `journal.append`, so the turn end the Hub folds is the one a live
//! `claude-sdk` worker produces. Two owner devices (phone, desk) log in
//! separately: seen is per owner, so both always read the same `unseen`.
//! A global `/v1/follow` socket observes the `instance.seen` broadcast.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_driver::{DriverKind, StdoutMapper};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8600-8600000000c5";
const TIMEOUT: Duration = Duration::from_secs(10);

type Follow = tokio_tungstenite::WebSocketStream<TcpStream>;

/// Raw HTTP returning status, response head and body.
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

/// Log a device in with the bootstrap token; returns its session cookie.
async fn login(hub: &remuda_hub::RunningHub, name: &str, kind: &str) -> String {
    let body = json!({
        "bootstrapToken": hub.bootstrap_token,
        "deviceName": name,
        "deviceKind": kind,
    })
    .to_string();
    let (status, head, rest) = raw_http_full(hub.addr, "POST", "/v1/login", &[], Some(&body)).await;
    assert_eq!(status, 200, "login {name}: {rest}");
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|l| {
            l.split_once(':')
                .map(|(_, v)| v.trim().split(';').next().unwrap().trim().to_owned())
        })
        .expect("set-cookie")
}

/// A Node that answers every Hub RPC and appends what the test hands it.
struct FakeNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<String>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value, tokio::sync::oneshot::Sender<()>)>,
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
                // A fence-capable Node: the Main seat is a continuity lineage.
                "params": { "hostId": host, "capabilities": {"lineageFence": true}, "host": {
                    "hostname":"seen-node","workspaces":[], "workspaceRevision":0,
                    "herdr":{"path":"/usr/bin/herdr"},
                    "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged-in"}]
                }}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let hello: Value = serde_json::from_str(&t).unwrap();
                assert!(hello.get("result").is_some(), "hello failed: {hello}");
            }
            other => panic!("hello: {other:?}"),
        }
        let (ftx, frx) = tokio::sync::mpsc::unbounded_channel();
        let (atx, mut arx) = tokio::sync::mpsc::unbounded_channel::<(
            String,
            Value,
            tokio::sync::oneshot::Sender<()>,
        )>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((id, event, done)) = arx.recv() => {
                        ws.send(Message::Text(json!({
                            "jsonrpc":"2.0","id":"append","method":"journal.append",
                            "params":{"instanceId":id,"event":event}
                        }).to_string().into())).await.unwrap();
                        // Hub RPCs may interleave with the append reply.
                        loop {
                            let Some(Ok(Message::Text(t))) = ws.next().await else { return };
                            let f: Value = serde_json::from_str(&t).unwrap();
                            if f["id"] == json!("append") {
                                assert!(f.get("result").is_some(), "append failed: {f}");
                                let _ = done.send(());
                                break;
                            }
                            if let Some(method) = f["method"].as_str() {
                                let _ = ftx.send(method.to_owned());
                                ws.send(Message::Text(json!({
                                    "jsonrpc":"2.0","id":f["id"],"result":{"accepted":true}
                                }).to_string().into())).await.unwrap();
                            }
                        }
                    }
                    frame = ws.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let f: Value = serde_json::from_str(&text).unwrap();
                                let Some(method) = f["method"].as_str() else { continue };
                                let _ = ftx.send(method.to_owned());
                                ws.send(Message::Text(json!({
                                    "jsonrpc":"2.0","id":f["id"],"result":{"accepted":true}
                                }).to_string().into())).await.unwrap();
                            }
                            Some(Ok(Message::Close(_))) | None => return,
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

    /// Wait for the Hub to send `method`, skipping other RPCs (closes and
    /// resumes the fake answers on its own).
    async fn wait_method(&mut self, method: &str) {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let seen = self.frames.recv().await.expect("frame channel closed");
                if seen == method {
                    return;
                }
            }
        })
        .await
        .with_context(|| format!("node frame {method}"))
        .unwrap();
    }

    /// Append one observation and wait for the Hub's journal.append reply, so
    /// the row is durable (and folded) when this returns.
    async fn append(&self, id: &str, event: Value) {
        let (done, rx) = tokio::sync::oneshot::channel();
        self.appends.send((id.to_owned(), event, done)).unwrap();
        tokio::time::timeout(TIMEOUT, rx)
            .await
            .context("append reply")
            .unwrap()
            .expect("fake node stopped");
    }
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self._task.abort();
    }
}

/// The real claude-sdk stdout mapper, driving one root turn at a time.
struct Worker {
    mapper: StdoutMapper,
    turns: u64,
}

impl Worker {
    fn new() -> Self {
        Self {
            mapper: StdoutMapper::new(DriverKind::ClaudeSdk, SESSION),
            turns: 0,
        }
    }

    /// What the driver journals when the user frame is written.
    fn send(&mut self) -> Value {
        self.mapper.begin_turn();
        let started = self
            .mapper
            .turn_started_observation(&format!("msg-seen-{}", self.turns))
            .expect("turn_started");
        serde_json::to_value(started).unwrap()
    }

    /// What the driver journals for the native `result` that ends the turn.
    fn finish(&mut self) -> Value {
        let index = self.turns;
        self.turns += 1;
        let frame = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": format!("turn {index} done"),
            "stop_reason": "end_turn",
            "session_id": SESSION,
            "result_index": index
        });
        let result = self
            .mapper
            .map(frame)
            .expect("map result")
            .into_iter()
            .map(|o| serde_json::to_value(o).unwrap())
            .find(|v| v.pointer("/payload/nativeName") == Some(&json!("result")))
            .expect("a result observation");
        assert_eq!(
            result.pointer("/payload/relatedIds/settledRootTurn"),
            Some(&json!("true")),
            "the real mapper settles this root turn"
        );
        result
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    /// Two owner devices: seen is per owner, so both read the same value.
    phone: String,
    desk: String,
    host: String,
}

impl Ctx {
    async fn boot() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hub = spawn(HubConfig::for_test(dir.path().join("data")))
            .await
            .unwrap();
        let phone = login(&hub, "seen-phone", "human").await;
        let desk = login(&hub, "seen-desk", "human").await;
        Self {
            _dir: dir,
            hub,
            phone,
            desk,
            host: HostId::new().as_id().as_str().to_owned(),
        }
    }

    async fn http(
        &self,
        cookie: &str,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (u16, Value) {
        self.http_with(cookie, &[], method, path, body).await
    }

    async fn http_with(
        &self,
        cookie: &str,
        extra: &[(&str, &str)],
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (u16, Value) {
        let mut headers = vec![("Cookie", cookie)];
        headers.extend_from_slice(extra);
        let (status, _, rest) = raw_http_full(self.hub.addr, method, path, &headers, body).await;
        (
            status,
            serde_json::from_str(rest.trim()).unwrap_or(json!({ "raw": rest })),
        )
    }

    /// A running claude-sdk instance on the fake Node.
    async fn running_instance(&self, node: &mut FakeNode) -> String {
        self.running_with(node, json!({})).await
    }

    /// A running claude-sdk instance created with `extra` create fields.
    async fn running_with(&self, node: &mut FakeNode, extra: Value) -> String {
        let mut body = json!({
            "hostId": self.host,
            "kind": "claude",
            "driver": "claude-sdk",
            "permissionMode": "auto",
            "prompt": "seen-hub"
        });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        let (status, created) = self
            .http(
                &self.phone,
                "POST",
                "/v1/instances",
                Some(&body.to_string()),
            )
            .await;
        assert_eq!(status, 200, "{created}");
        let id = created["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_owned();
        node.wait_method("instance.create").await;
        self.session_started(node, &id).await;
        id
    }

    /// The driver's native session report: the instance is running.
    async fn session_started(&self, node: &FakeNode, id: &str) {
        node.append(
            id,
            json!({
                "kind": "lifecycle",
                "payload": {
                    "type": "native", "topic": "session", "nativeName": "session",
                    "nativeId": { "state": "known", "value": SESSION },
                    "status": { "state": "known", "value": "started" },
                    "relatedIds": {}, "dataRef": null, "severity": "info",
                    "affectsCompletion": false
                }
            }),
        )
        .await;
    }

    /// `POST /v1/instances/{id}/resume`; returns the new instance's id.
    async fn resume(&self, id: &str) -> String {
        let (status, resumed) = self
            .http(
                &self.phone,
                "POST",
                &format!("/v1/instances/{id}/resume"),
                Some(r#"{"mode":"structured"}"#),
            )
            .await;
        assert_eq!(status, 200, "{resumed}");
        let next = resumed["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(next, id, "a new instance: {resumed}");
        next
    }

    /// The instance row as this device's list poll sees it.
    async fn row(&self, cookie: &str, id: &str) -> Value {
        let (status, page) = self.http(cookie, "GET", "/v1/instances", None).await;
        assert_eq!(status, 200, "{page}");
        page["items"]
            .as_array()
            .and_then(|items| items.iter().find(|row| row["instanceId"] == json!(id)))
            .cloned()
            .unwrap_or_else(|| panic!("{id} missing from the list: {page}"))
    }

    async fn seen(&self, cookie: &str, id: &str, body: Value) -> (u16, Value) {
        self.http(
            cookie,
            "POST",
            &format!("/v1/instances/{id}/seen"),
            Some(&body.to_string()),
        )
        .await
    }

    async fn follow(&self, cookie: &str) -> Follow {
        let mut req = format!("ws://{}/v1/follow", self.hub.addr)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert("Cookie", cookie.parse().unwrap());
        let tcp = TcpStream::connect(self.hub.addr).await.unwrap();
        let (follow, _) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
        follow
    }
}

/// Next `instance.seen` event for `id` on a global follower.
async fn next_seen(follow: &mut Follow, id: &str) -> Value {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let Some(Ok(msg)) = follow.next().await else {
                panic!("follower closed");
            };
            let Message::Text(text) = msg else { continue };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["type"] == json!("event")
                && frame["event"]["type"] == json!("instance.seen")
                && frame["event"]["instanceId"] == json!(id)
            {
                // Global-only: never addressed to the instance's journal.
                assert_eq!(frame["instanceId"], json!(""));
                return frame["event"].clone();
            }
        }
    })
    .await
    .expect("an instance.seen broadcast")
}

fn seq(value: &Value) -> u64 {
    value
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("not a seq: {value}"))
}

#[tokio::test]
async fn a_real_turn_end_is_unseen_for_every_owner_device_until_seen() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.running_instance(&mut node).await;
    let mut follow = ctx.follow(&ctx.desk).await;
    let mut worker = Worker::new();

    // A turn in flight is not "done".
    node.append(&id, worker.send()).await;
    let row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(row["activity"], json!("working"));
    assert_eq!(row["unseen"], json!(false));
    assert!(row.get("turnEndSeq").is_none(), "no turn end yet: {row}");

    // The worker finishes while no client shows the session.
    node.append(&id, worker.finish()).await;
    let phone_row = ctx.row(&ctx.phone, &id).await;
    let desk_row = ctx.row(&ctx.desk, &id).await;
    assert_eq!(phone_row["activity"], json!("idle"));
    assert_eq!(phone_row["unseen"], json!(true), "{phone_row}");
    assert_eq!(
        seq(&phone_row["turnEndSeq"]),
        seq(&phone_row["durableSeq"]),
        "the turn end is the result row"
    );
    assert_eq!(desk_row["unseen"], json!(true), "per owner, not per device");
    let broadcast = next_seen(&mut follow, &id).await;
    assert_eq!(broadcast["unseen"], json!(true));
    assert_eq!(broadcast["turnEndSeq"], phone_row["turnEndSeq"]);

    let durable = phone_row["durableSeq"].clone();
    let (status, view) = ctx
        .seen(&ctx.desk, &id, json!({ "durableSeq": durable }))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["unseen"], json!(false));
    assert_eq!(view["seenSeq"], durable);
    assert!(view["seenAt"].is_string());
    let broadcast = next_seen(&mut follow, &id).await;
    assert_eq!(broadcast["unseen"], json!(false));
    assert_eq!(broadcast["seenSeq"], durable);
    // Seen from the desk clears it for the phone on its very next poll.
    let phone_row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(phone_row["unseen"], json!(false), "{phone_row}");
    assert_eq!(phone_row["seenSeq"], durable);
    let (_, single) = ctx
        .http(&ctx.phone, "GET", &format!("/v1/instances/{id}"), None)
        .await;
    assert_eq!(single["unseen"], json!(false));

    // An idle status that repeats on an idle row is no turn end: it neither
    // re-arms ◉ nor moves the turn-end mark.
    node.append(
        &id,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native", "topic": "session", "nativeName": "agent_status",
                "nativeId": { "state": "known", "value": SESSION },
                "status": { "state": "known", "value": "idle" },
                "relatedIds": {}, "dataRef": null, "severity": "info",
                "affectsCompletion": false
            }
        }),
    )
    .await;
    let row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(row["unseen"], json!(false), "{row}");
    assert_eq!(row["turnEndSeq"], phone_row["turnEndSeq"]);
    assert!(seq(&row["durableSeq"]) > seq(&durable));

    // The NEXT turn end is newer than the seen seq: unseen again.
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    let row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(row["unseen"], json!(true), "{row}");
    assert!(seq(&row["turnEndSeq"]) > seq(&durable));
    Ok(())
}

#[tokio::test]
async fn seen_is_monotonic_and_clamped_to_the_journal() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.running_instance(&mut node).await;
    let mut worker = Worker::new();
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    let row = ctx.row(&ctx.phone, &id).await;
    let durable = seq(&row["durableSeq"]);
    let end = seq(&row["turnEndSeq"]);
    assert!(durable >= 3, "session + start + result: {row}");

    // A report below the turn end leaves it unseen.
    let (status, view) = ctx
        .seen(
            &ctx.phone,
            &id,
            json!({ "durableSeq": (end - 1).to_string() }),
        )
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["unseen"], json!(true));
    assert_eq!(seq(&view["seenSeq"]), end - 1);

    // A JSON integer is accepted; this one covers the turn end.
    let (_, view) = ctx
        .seen(&ctx.phone, &id, json!({ "durableSeq": end }))
        .await;
    assert_eq!(view["unseen"], json!(false));
    assert_eq!(seq(&view["seenSeq"]), end);

    // A lower report never moves seen back, from any owner device.
    for lower in [json!("1"), json!(0)] {
        let (status, view) = ctx
            .seen(&ctx.desk, &id, json!({ "durableSeq": lower }))
            .await;
        assert_eq!(status, 200, "{view}");
        assert_eq!(seq(&view["seenSeq"]), end, "seen never moves back");
        assert_eq!(view["unseen"], json!(false));
    }
    assert_eq!(seq(&ctx.row(&ctx.desk, &id).await["seenSeq"]), end);

    // A seq past the journal is clamped to the durable seq, so it cannot
    // pre-mark turns that have not happened yet.
    let (_, view) = ctx
        .seen(&ctx.phone, &id, json!({ "durableSeq": "999999" }))
        .await;
    assert_eq!(seq(&view["seenSeq"]), durable);
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    assert_eq!(ctx.row(&ctx.phone, &id).await["unseen"], json!(true));

    // Malformed and unknown targets.
    let (status, _) = ctx
        .seen(&ctx.phone, &id, json!({ "durableSeq": "abc" }))
        .await;
    assert_eq!(status, 400);
    let (status, _) = ctx
        .seen(&ctx.phone, &id, json!({ "durableSeq": "-1" }))
        .await;
    assert_eq!(status, 400);
    let (status, _) = ctx
        .seen(&ctx.phone, "ins_missing", json!({ "durableSeq": "1" }))
        .await;
    assert_eq!(status, 404);
    Ok(())
}

#[tokio::test]
async fn a_turn_that_ends_while_an_owner_client_shows_it_never_reads_unseen() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.running_instance(&mut node).await;
    let mut follow = ctx.follow(&ctx.phone).await;
    let mut worker = Worker::new();

    // The desk shows the session page while the turn runs and ends.
    node.append(&id, worker.send()).await;
    let durable = ctx.row(&ctx.desk, &id).await["durableSeq"].clone();
    let (status, view) = ctx
        .seen(
            &ctx.desk,
            &id,
            json!({ "durableSeq": durable, "visible": true }),
        )
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(next_seen(&mut follow, &id).await["seenSeq"], durable);
    node.append(&id, worker.finish()).await;
    let row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(row["activity"], json!("idle"));
    assert_eq!(row["unseen"], json!(false), "watched turn end: {row}");
    assert_eq!(
        row["seenSeq"], row["turnEndSeq"],
        "folded seen in the append itself"
    );
    // The fold is broadcast too, so other clients' rows advance.
    let broadcast = next_seen(&mut follow, &id).await;
    assert_eq!(broadcast["seenSeq"], row["turnEndSeq"]);
    assert_eq!(broadcast["unseen"], json!(false));

    // Leases are per device: the phone also showed it, then left. The desk
    // still shows it (and renews, as a visible client does), so the next
    // turn end is still absorbed.
    let (_, _) = ctx
        .seen(
            &ctx.desk,
            &id,
            json!({ "durableSeq": row["durableSeq"], "visible": true }),
        )
        .await;
    let (_, _) = ctx
        .seen(
            &ctx.phone,
            &id,
            json!({ "durableSeq": row["durableSeq"], "visible": true }),
        )
        .await;
    let (_, _) = ctx
        .seen(
            &ctx.phone,
            &id,
            json!({ "durableSeq": row["durableSeq"], "visible": false }),
        )
        .await;
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    let row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(row["unseen"], json!(false), "desk still visible: {row}");

    // Once the desk leaves too, a turn that ends is unseen.
    let (_, _) = ctx
        .seen(
            &ctx.desk,
            &id,
            json!({ "durableSeq": row["durableSeq"], "visible": false }),
        )
        .await;
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    let row = ctx.row(&ctx.desk, &id).await;
    assert_eq!(row["unseen"], json!(true), "nobody watching: {row}");

    // A report without `visible` (e.g. "mark all seen" from a list) clears
    // it but holds no lease: the following turn end is unseen again.
    let (_, view) = ctx
        .seen(&ctx.phone, &id, json!({ "durableSeq": row["durableSeq"] }))
        .await;
    assert_eq!(view["unseen"], json!(false));
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    assert_eq!(ctx.row(&ctx.desk, &id).await["unseen"], json!(true));
    Ok(())
}

#[tokio::test]
async fn only_human_owner_devices_may_mark_seen() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let id = ctx.running_instance(&mut node).await;
    let mut worker = Worker::new();
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    let durable = ctx.row(&ctx.phone, &id).await["durableSeq"].clone();
    let body = json!({ "durableSeq": durable, "visible": true }).to_string();
    let path = format!("/v1/instances/{id}/seen");

    // An Agent credential (a device narrowed to an instance) is refused by
    // the agent-route policy — even for its own instance.
    let (status, _) = ctx
        .http_with(
            &ctx.phone,
            &[("x-remuda-instance-id", id.as_str())],
            "POST",
            &path,
            Some(&body),
        )
        .await;
    assert_eq!(status, 403, "agent credential");

    // A Bot device is the owner's courier, not the owner's eyes.
    let bot = login(&ctx.hub, "seen-bot", "bot").await;
    let (status, _) = ctx.http(&bot, "POST", &path, Some(&body)).await;
    assert_eq!(status, 403, "bot device");

    // Unauthenticated.
    let (status, _, _) = raw_http_full(ctx.hub.addr, "POST", &path, &[], Some(&body)).await;
    assert_eq!(status, 401);

    // Nothing above moved seen or took a visibility lease.
    let row = ctx.row(&ctx.phone, &id).await;
    assert_eq!(row["unseen"], json!(true), "{row}");
    assert!(row.get("seenSeq").is_none(), "{row}");
    node.append(&id, worker.send()).await;
    node.append(&id, worker.finish()).await;
    assert_eq!(ctx.row(&ctx.phone, &id).await["unseen"], json!(true));

    // The owner's own device may.
    let (status, view) = ctx.http(&ctx.phone, "POST", &path, Some(&body)).await;
    assert_eq!(status, 200, "{view}");
    Ok(())
}

#[tokio::test]
async fn a_resumed_child_and_a_main_chapter_start_clean() -> Result<()> {
    let ctx = Ctx::boot().await;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await;
    let exited = json!({
        "kind": "lifecycle",
        "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" }
    });

    // D-026: the parent finished a turn nobody saw, then exited; the owner
    // resumes it. The child is a new id with no seen row and no turn end: it
    // reads unseen only once a turn of its own ends. The parent keeps its own
    // state (the home shows ◉ only on a live idle row).
    let parent = ctx.running_instance(&mut node).await;
    let mut worker = Worker::new();
    node.append(&parent, worker.send()).await;
    node.append(&parent, worker.finish()).await;
    node.append(&parent, exited.clone()).await;
    let child = ctx.resume(&parent).await;
    let parent_row = ctx.row(&ctx.phone, &parent).await;
    let child_row = ctx.row(&ctx.phone, &child).await;
    assert_eq!(parent_row["unseen"], json!(true), "{parent_row}");
    assert_eq!(child_row["unseen"], json!(false), "{child_row}");
    assert!(child_row.get("turnEndSeq").is_none(), "{child_row}");
    assert!(child_row.get("seenSeq").is_none(), "{child_row}");
    // The child's own first turn end is what makes it unseen.
    ctx.session_started(&node, &child).await;
    let mut child_worker = Worker::new();
    node.append(&child, child_worker.send()).await;
    node.append(&child, child_worker.finish()).await;
    let child_row = ctx.row(&ctx.desk, &child).await;
    assert_eq!(child_row["unseen"], json!(true), "{child_row}");
    assert_eq!(child_row["turnEndSeq"], child_row["durableSeq"]);

    // D-057: the Main seat (address-owner) finished a turn nobody saw; its
    // lineage continues into a new chapter, which also starts clean.
    let main = ctx
        .running_with(
            &mut node,
            json!({
                "grants": ["address-owner"],
                "restart": { "onProcessLoss": true, "maxPerHour": 3 }
            }),
        )
        .await;
    let mut main_worker = Worker::new();
    node.append(&main, main_worker.send()).await;
    node.append(&main, main_worker.finish()).await;
    assert_eq!(ctx.row(&ctx.phone, &main).await["unseen"], json!(true));
    // D-057 §6.5: Resume runs on a paused lineage — Pause (close) first.
    let (status, paused) = ctx
        .http(
            &ctx.phone,
            "POST",
            &format!("/v1/instances/{main}/commands"),
            Some(r#"{"operation":"instance.close","payload":{}}"#),
        )
        .await;
    assert_eq!(status, 200, "{paused}");
    let chapter2 = ctx.resume(&main).await;
    let fenced = ctx.row(&ctx.phone, &main).await;
    let chapter2_row = ctx.row(&ctx.desk, &chapter2).await;
    assert_eq!(chapter2_row["lineageId"], fenced["lineageId"]);
    assert_eq!(chapter2_row["generation"], json!(2));
    assert_eq!(chapter2_row["unseen"], json!(false), "{chapter2_row}");
    assert!(chapter2_row.get("turnEndSeq").is_none(), "{chapter2_row}");
    // The new chapter's own turn end lights it; seeing it clears only it.
    ctx.session_started(&node, &chapter2).await;
    let mut chapter_worker = Worker::new();
    node.append(&chapter2, chapter_worker.send()).await;
    node.append(&chapter2, chapter_worker.finish()).await;
    let chapter2_row = ctx.row(&ctx.phone, &chapter2).await;
    assert_eq!(chapter2_row["unseen"], json!(true), "{chapter2_row}");
    let (_, view) = ctx
        .seen(
            &ctx.desk,
            &chapter2,
            json!({ "durableSeq": chapter2_row["durableSeq"] }),
        )
        .await;
    assert_eq!(view["unseen"], json!(false));
    assert_eq!(
        ctx.row(&ctx.phone, &main).await["unseen"],
        json!(true),
        "the fenced chapter keeps its own state"
    );
    Ok(())
}
