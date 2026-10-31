//! D-057 §7.10 (ma-fence), the Hub half: the fence transaction F, the send
//! gate, fence delivery and reconnect ordering, outcome resolution, close
//! becomes pause and Resume only from paused — against fake Nodes, with
//! deterministic hooks (race barriers and continuation hooks) and no
//! millisecond thresholds. The Node half of each row lives in
//! `crates/remuda-node/src/fence/tests.rs`.
#![allow(missing_docs)]

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8000-0000000000bb";

/// How a fake Node answers `lineage.fence`.
#[derive(Clone)]
enum FenceReply {
    /// Acknowledge with these `(opId, outcome)` pairs.
    Ack(Vec<(String, String)>),
    /// Do not answer until the test releases it.
    Hold(Arc<tokio::sync::Notify>),
}

#[derive(Default)]
struct Script {
    fence: Option<FenceReply>,
    /// Per-delivery acknowledgements, consumed one per `lineage.fence` frame
    /// before `fence` applies.
    fence_queue: std::collections::VecDeque<Vec<(String, String)>>,
    /// Methods answered with a JSON-RPC error `(code, message)`.
    errors: HashMap<String, (i64, String)>,
}

struct FakeNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    script: Arc<Mutex<Script>>,
    node_token: Option<String>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl FakeNode {
    async fn connect(
        hub: &remuda_hub::RunningHub,
        host: &str,
        fence_capable: bool,
    ) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        Self::connect_with(hub, host, &enroll, fence_capable, json!({})).await
    }

    async fn connect_with(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        fence_capable: bool,
        extra: Value,
    ) -> Result<Self> {
        Self::connect_inner(hub, host, bearer, fence_capable, extra, Script::default()).await
    }

    /// Reconnect with acknowledgements scripted BEFORE the hello, so the
    /// fences delivered right after it are answered deterministically.
    async fn connect_scripted(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        acks: Vec<Vec<(String, String)>>,
    ) -> Result<Self> {
        let script = Script {
            fence_queue: acks.into_iter().collect(),
            ..Script::default()
        };
        Self::connect_inner(hub, host, bearer, true, json!({}), script).await
    }

    async fn connect_inner(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        fence_capable: bool,
        extra: Value,
        script: Script,
    ) -> Result<Self> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut params = json!({ "hostId": host, "host": {
            "hostname": format!("{host}.fence"), "workspaces": [], "workspaceRevision": 0,
            "herdr": {"path": "/usr/bin/herdr"},
            "cli": [{"kind": "claude", "path": "/usr/bin/claude", "auth": "logged_in"}]
        }});
        if fence_capable {
            params["capabilities"] = json!({ "lineageFence": true });
        }
        if let (Some(object), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        node.send(Message::Text(
            json!({"jsonrpc":"2.0","id":"hello","method":"node.hello","params":params})
                .to_string()
                .into(),
        ))
        .await?;
        let hello: Value =
            serde_json::from_str(&node.next().await.context("hello")??.into_text()?)?;
        assert!(hello.get("result").is_some(), "hello failed: {hello}");
        let node_token = hello["result"]["nodeToken"].as_str().map(str::to_owned);
        let script = Arc::new(Mutex::new(script));
        let (frame_tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let (appends, mut append_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let task_script = Arc::clone(&script);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((instance_id, event)) = append_rx.recv() => {
                        let frame = json!({"jsonrpc":"2.0","id":"append","method":"journal.append",
                            "params":{"instanceId":instance_id,"event":event}});
                        if node.send(Message::Text(frame.to_string().into())).await.is_err() { break; }
                    }
                    Some(reply) = out_rx.recv() => {
                        if node.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                    }
                    frame = node.next() => {
                        let Some(Ok(Message::Text(text))) = frame else {
                            if matches!(frame, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { break; }
                            continue;
                        };
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                        let Some(method) = frame["method"].as_str().map(str::to_owned) else { continue };
                        let id = frame["id"].clone();
                        let params = frame["params"].clone();
                        if frame_tx.send((method.clone(), params.clone())).is_err() { break; }
                        if id.is_null() { continue; }
                        let (fence, error) = {
                            let mut script = task_script.lock().unwrap();
                            let queued = if method == "lineage.fence" {
                                script.fence_queue.pop_front().map(FenceReply::Ack)
                            } else {
                                None
                            };
                            (queued.or_else(|| script.fence.clone()), script.errors.get(&method).cloned())
                        };
                        let reply = if let Some((code, message)) = error {
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
                        } else if method == "lineage.fence" {
                            let outcomes = match &fence {
                                Some(FenceReply::Hold(release)) => {
                                    let release = Arc::clone(release);
                                    let out = out_tx.clone();
                                    let fence_id = params["fenceId"].clone();
                                    tokio::spawn(async move {
                                        release.notified().await;
                                        let _ = out.send(json!({"jsonrpc":"2.0","id":id,
                                            "result":{"fenceId":fence_id,"closedInstanceIds":[],"outcomes":[]}}));
                                    });
                                    continue;
                                }
                                Some(FenceReply::Ack(pairs)) => pairs
                                    .iter()
                                    .map(|(op, outcome)| json!({"opId": op, "outcome": outcome}))
                                    .collect::<Vec<_>>(),
                                None => Vec::new(),
                            };
                            json!({"jsonrpc":"2.0","id":id,"result":{
                                "fenceId": params["fenceId"], "closedInstanceIds": [], "outcomes": outcomes}})
                        } else {
                            json!({"jsonrpc":"2.0","id":id,"result":{"accepted":true}})
                        };
                        if node.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                    }
                }
            }
        });
        Ok(Self {
            frames,
            appends,
            script,
            node_token,
            task: Some(task),
        })
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap()
    }

    async fn next_frame(&mut self) -> Result<(String, Value)> {
        tokio::time::timeout(Duration::from_secs(10), self.frames.recv())
            .await?
            .context("frame")
    }

    /// The next frame that is not a `lineage.fence`.
    async fn next_work_frame(&mut self) -> Result<(String, Value)> {
        loop {
            let (method, params) = self.next_frame().await?;
            if method != "lineage.fence" {
                return Ok((method, params));
            }
        }
    }

    /// Drain everything already queued, returning the methods seen.
    fn drain(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(frame) = self.frames.try_recv() {
            out.push(frame);
        }
        out
    }

    fn disconnect(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    seat_host: String,
    worker_host: String,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let hub = spawn(HubConfig::for_test(data.clone())).await?;
        let human = hub.mint_device_token("fence-phone").await?;
        Ok(Self {
            _dir: dir,
            data,
            hub,
            http: reqwest::Client::new(),
            human,
            seat_host: HostId::new().as_id().as_str().to_owned(),
            worker_host: HostId::new().as_id().as_str().to_owned(),
        })
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        Ok(rusqlite::Connection::open(self.data.join("hub.sqlite"))?)
    }

    /// The continuity seat (address-owner + dispatch, C1 restart) on the
    /// seat host; returns (id, launch token).
    async fn seat(&self, node: &mut FakeNode) -> Result<(String, String)> {
        let created: Value = self
            .http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(&self.human)
            .json(&json!({
                "hostId": self.seat_host, "kind": "claude", "driver": "claude-sdk",
                "name": "main", "title": "Main", "grants": ["address-owner", "dispatch"],
                "permissionMode": "manual",
                // An explicitly narrowed scope lets the seat message the
                // in-scope worker without a per-message approval.
                "scope": {"hostIds": [self.seat_host, self.worker_host]},
                "restart": {"onProcessLoss": true, "maxPerHour": 3},
                "prompt": "seat brief",
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("seat")?
            .to_owned();
        let (method, params) = node.next_work_frame().await?;
        assert_eq!(method, "instance.create");
        let token = params["agentCredential"]["token"]
            .as_str()
            .context("token")?
            .to_owned();
        Ok((id, token))
    }

    /// A worker on the worker host the seat may message (owner-created; a
    /// seat create on another host would need the D-017 approval).
    async fn worker(&self, _seat_token: &str, node: &mut FakeNode) -> Result<String> {
        let created: Value = self
            .http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(&self.human)
            .json(&json!({
                "hostId": self.worker_host, "kind": "claude", "driver": "claude-print",
                "permissionMode": "manual", "prompt": "worker brief",
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("worker")?
            .to_owned();
        let (method, _) = node.next_work_frame().await?;
        assert_eq!(method, "instance.create");
        Ok(id)
    }

    async fn command(&self, token: &str, target: &str, body: Value) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}/v1/instances/{target}/commands", self.base()))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?)
    }

    async fn send(
        &self,
        token: &str,
        target: &str,
        command_id: &str,
        text: &str,
    ) -> Result<reqwest::Response> {
        self.command(
            token,
            target,
            json!({"operation": "instance.send", "commandId": command_id,
                   "payload": {"input": {"type": "prompt", "text": text}}}),
        )
        .await
    }

    /// Pause = the existing close of the lineage's chapter (§6.5).
    async fn pause(&self, token: &str, id: &str) -> Result<reqwest::Response> {
        self.command(
            token,
            id,
            json!({"operation": "instance.close", "payload": {}}),
        )
        .await
    }

    async fn lineage(&self, id: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}/v1/lineages/{id}", self.base()))
            .bearer_auth(&self.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn command_row(&self, id: &str) -> Result<(String, Option<String>, Option<String>)> {
        let db = self.db()?;
        Ok(db.query_row(
            "SELECT state, settlement_outcome, settlement_reason FROM commands WHERE id = ?1",
            rusqlite::params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }

    async fn report_session(&self, node: &FakeNode, id: &str) -> Result<()> {
        node.appends.send((
            id.to_owned(),
            json!({"kind":"lifecycle","payload":{"type":"native","topic":"session",
                "nativeName":"session","nativeId":{"state":"known","value":SESSION},
                "status":{"state":"known","value":"started"},"relatedIds":{},"dataRef":null,
                "severity":"info","affectsCompletion":false}}),
        ))?;
        self.wait_until(|| async {
            let view: Value = self
                .http
                .get(format!("{}/v1/instances/{id}", self.base()))
                .bearer_auth(&self.human)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            (view["nativeSessionId"] == json!(SESSION)).then_some(())
        })
        .await
    }

    async fn wait_until<F, Fut, T>(&self, probe: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(value) = probe().await {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("condition never held")
    }

    /// The fence op outcome for `op_id` in the lineage record.
    async fn op_outcome(&self, lineage: &str, op_id: &str) -> Result<Option<String>> {
        let record = self.lineage(lineage).await?;
        Ok(record["fences"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|fence| fence["operations"].as_array().cloned().unwrap_or_default())
            .find(|op| op["opId"] == op_id)
            .and_then(|op| op["outcome"].as_str().map(str::to_owned)))
    }
}

fn cmd_id() -> String {
    remuda_protocol::CommandId::new()
        .as_id()
        .as_str()
        .to_owned()
}

fn initiator_of(ctx: &Ctx, seat: &str) -> Result<remuda_protocol::Initiator> {
    let db = ctx.db()?;
    let (lineage, generation): (String, i64) = db.query_row(
        "SELECT lineage_id, generation FROM instances WHERE id = ?1",
        rusqlite::params![seat],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(remuda_protocol::Initiator {
        instance_id: seat.to_owned(),
        lineage_id: lineage,
        generation,
    })
}

// ── Close becomes pause; paused_by; credentials ─────────────────────────

/// §6.3/§6.5 + §7.2: a Human close of the seat is F without successor at
/// admission — paused{device} with paused_by/paused_at written, the
/// predecessor fenced, its launch credential and MCP token deleted (401),
/// the fence delivered BEFORE the close is forwarded.
#[tokio::test]
async fn a_human_close_pauses_the_lineage_records_paused_by_and_revokes_credentials() -> Result<()>
{
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let (seat, token) = ctx.seat(&mut node).await?;
    let mcp: Value = ctx
        .http
        .post(format!("{}/v1/instances/{seat}/mcp-token", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mcp = mcp["token"].as_str().context("mcp")?.to_owned();

    let paused = ctx.pause(&ctx.human, &seat).await?;
    assert_eq!(paused.status(), 200);
    let paused: Value = paused.json().await?;
    assert_eq!(paused["paused"], json!(true));
    // The fence is the first frame, then the close.
    let (first, fence) = node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    assert_eq!(fence["lineageId"], json!(seat));
    assert_eq!(fence["liveGeneration"], json!("2"));
    let (second, close) = node.next_frame().await?;
    assert_eq!(second, "instance.close");
    assert_eq!(close["instanceId"], json!(seat));

    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("paused"));
    assert_eq!(record["generation"], json!(2));
    assert_eq!(record["pausedBy"]["kind"], json!("device"));
    assert!(record["pausedBy"]["deviceId"].as_str().is_some());
    assert!(record["pausedAt"].as_str().is_some());
    assert_eq!(record["fences"][0]["reason"], json!("paused"));
    assert_eq!(record["fences"][0]["pendingHosts"], json!([]));

    for credential in [&token, &mcp] {
        let read = ctx
            .http
            .get(format!("{}/v1/instances/{seat}", ctx.base()))
            .bearer_auth(credential)
            .send()
            .await?;
        assert_eq!(read.status(), 401, "a fenced chapter's credential is gone");
    }
    // A second close of a paused lineage is the plain close (no new fence).
    let again = ctx.pause(&ctx.human, &seat).await?;
    assert_eq!(again.status(), 200);
    assert_eq!(ctx.lineage(&seat).await?["generation"], json!(2));
    Ok(())
}

/// §6.3: Resume only from paused (409 otherwise); DELETE of an unpaused
/// current chapter is 409; after Pause, Resume starts a new chapter at the
/// live generation and a send addressed to the superseded chapter is
/// refused and never reaches the Node.
#[tokio::test]
async fn resume_only_from_paused_and_a_send_to_the_superseded_chapter_never_leaves_the_hub()
-> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let (seat, _token) = ctx.seat(&mut node).await?;
    ctx.report_session(&node, &seat).await?;

    let refused = ctx
        .http
        .post(format!("{}/v1/instances/{seat}/resume", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({}))
        .send()
        .await?;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        refused.json::<Value>().await?["reason"],
        json!("not-paused")
    );
    let delete = ctx
        .http
        .delete(format!("{}/v1/instances/{seat}?force=1", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(delete.status(), 409);
    assert_eq!(delete.json::<Value>().await?["reason"], json!("not-paused"));

    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    node.drain();
    let resumed: Value = ctx
        .http
        .post(format!("{}/v1/instances/{seat}/resume", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let successor = resumed["instance"]["instanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    assert_ne!(successor, seat);
    assert_eq!(
        resumed["instance"]["generation"],
        json!(2),
        "consecutive chapters"
    );
    let (method, params) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["lineage"], json!({"id": seat, "generation": "2"}));
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("starting"));
    assert!(record["pausedBy"].is_null());

    // A human message addressed to the superseded chapter (the PWA's stale
    // tab) is a plain send to a closed chapter; a message stamped by the
    // superseded chapter's own authority is refused at admission.
    let stale = initiator_of(&ctx, &seat)?;
    let db = ctx.db()?;
    let live: i64 = db.query_row(
        "SELECT generation FROM lineages WHERE lineage_id = ?1",
        rusqlite::params![seat],
        |row| row.get(0),
    )?;
    assert!(
        stale.generation < live
            || db.query_row(
                "SELECT fenced_at IS NOT NULL FROM instances WHERE id = ?1",
                rusqlite::params![seat],
                |row| row.get::<_, bool>(0),
            )?
    );
    let error = ctx
        .hub
        .test_handle()
        .test_call_node_op(
            &ctx.seat_host,
            "worker.provision",
            "op-stale".into(),
            Some(stale),
            None,
        )
        .await
        .expect_err("a superseded chapter's op is refused");
    assert!(error.to_string().contains("fenced"), "{error}");
    assert!(
        node.drain()
            .iter()
            .all(|(method, _)| method != "worker.provision"),
        "nothing reached the Node"
    );
    Ok(())
}

// ── §7.10 rows ──────────────────────────────────────────────────────────

/// §7.10 "Held command": a seat command held for an offline worker host is
/// cancelled at the Hub by F; after the host reconnects the fence is the
/// first frame and a same-id replay reaches nothing.
#[tokio::test]
async fn a_held_command_is_cancelled_by_f_and_a_same_id_replay_reaches_nothing() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let worker_token = worker_node.node_token.clone().context("node token")?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    worker_node.disconnect();
    ctx.wait_until(|| async {
        let host: Value = ctx
            .http
            .get(format!("{}/v1/hosts/{}", ctx.base(), ctx.worker_host))
            .bearer_auth(&ctx.human)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (host["online"] == json!(false)).then_some(())
    })
    .await?;

    let held = cmd_id();
    let response: Value = ctx
        .send(&token, &worker, &held, "HELD")
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(response["command"]["state"], json!("queued"));

    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    assert_eq!(
        ctx.command_row(&held).await?,
        (
            "settled".into(),
            Some("rejected".into()),
            Some("fenced".into())
        )
    );
    assert_eq!(
        ctx.op_outcome(&seat, &held).await?.as_deref(),
        Some("cancelled-at-hub")
    );

    let mut worker_node =
        FakeNode::connect_with(&ctx.hub, &ctx.worker_host, &worker_token, true, json!({})).await?;
    let (first, fence) = worker_node.next_frame().await?;
    assert_eq!(
        first, "lineage.fence",
        "the fence is the first frame after reconnect"
    );
    assert_eq!(fence["lineageId"], json!(seat));
    // Same-id replay by the (fenced) seat credential: 401; by the owner on
    // the seat's behalf it replays the settled row and forwards nothing.
    let replay = ctx.send(&token, &worker, &held, "HELD").await?;
    assert_eq!(replay.status(), 401);
    ctx.wait_until(|| async {
        ctx.lineage(&seat)
            .await
            .ok()
            .and_then(|record| (record["fences"][0]["pendingHosts"] == json!([])).then_some(()))
    })
    .await?;
    assert!(
        worker_node
            .drain()
            .iter()
            .all(|(method, _)| method != "instance.send"),
        "the held prompt never reaches the worker"
    );
    Ok(())
}

/// §7.10 "Send gate": forwarding pauses after `mark_forward_intent`
/// committed until F committed; the gate withholds the frame.
#[tokio::test]
async fn the_send_gate_withholds_a_forward_intended_before_f() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;

    let command = cmd_id();
    let (reached, release) = ctx
        .hub
        .test_handle()
        .test_arm_forward_intent_barrier(&ctx.worker_host, &command);
    let send = {
        let ctx_http = ctx.http.clone();
        let url = format!("{}/v1/instances/{worker}/commands", ctx.base());
        let token = token.clone();
        let command = command.clone();
        tokio::spawn(async move {
            ctx_http
                .post(url)
                .bearer_auth(token)
                .json(&json!({"operation":"instance.send","commandId":command,
                    "payload":{"input":{"type":"prompt","text":"WITHHELD"}}}))
                .send()
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), reached.notified())
        .await
        .context("barrier never reached")?;
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    // The intent committed before F: potentially executed until the gate.
    assert_eq!(
        ctx.op_outcome(&seat, &command).await?.as_deref(),
        Some("unknown")
    );
    release.send(()).ok();
    let response: Value = send.await??.json().await?;
    assert_eq!(response["command"]["state"], json!("settled"), "{response}");
    assert_eq!(
        ctx.command_row(&command).await?,
        (
            "settled".into(),
            Some("withheld".into()),
            Some("fenced".into())
        )
    );
    assert_eq!(
        ctx.op_outcome(&seat, &command).await?.as_deref(),
        Some("withheld")
    );
    assert!(
        worker_node
            .drain()
            .iter()
            .all(|(method, _)| method != "instance.send"),
        "nothing reaches the Node"
    );
    Ok(())
}

/// §7.10 "Link order", Hub half: forwarding passed the send gate and parks
/// before the link write until the fence frame is enqueued; the command
/// frame follows the fence frame; the Node's `OWNER_FENCED` refusal settles
/// it `rejected(fenced)` and the fence record says refused-by-node.
#[tokio::test]
async fn a_frame_after_the_fence_frame_is_refused_by_the_node() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    worker_node
        .script()
        .errors
        .insert("instance.send".into(), (-32003, "fenced".into()));

    let command = cmd_id();
    let (reached, release) = ctx
        .hub
        .test_handle()
        .test_arm_forward_send_barrier(&ctx.worker_host, &command);
    let send = {
        let http = ctx.http.clone();
        let url = format!("{}/v1/instances/{worker}/commands", ctx.base());
        let token = token.clone();
        let command = command.clone();
        tokio::spawn(async move {
            http.post(url)
                .bearer_auth(token)
                .json(&json!({"operation":"instance.send","commandId":command,
                    "payload":{"input":{"type":"prompt","text":"LINK-ORDER"}}}))
                .send()
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), reached.notified())
        .await
        .context("barrier never reached")?;
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    let (first, _) = worker_node.next_frame().await?;
    assert_eq!(
        first, "lineage.fence",
        "the fence frame is on the link first"
    );
    release.send(()).ok();
    let _ = send.await?;
    let (second, params) = worker_node.next_frame().await?;
    assert_eq!(second, "instance.send");
    assert_eq!(params["commandId"], json!(command));
    assert_eq!(
        ctx.command_row(&command).await?,
        (
            "settled".into(),
            Some("rejected".into()),
            Some("fenced".into())
        )
    );
    assert_eq!(
        ctx.op_outcome(&seat, &command).await?.as_deref(),
        Some("refused-by-node")
    );
    Ok(())
}

/// §7.10 "Unacknowledged-fence window": while a host owes a fence
/// acknowledgement a caller passing `live=true` to `forward_if_online`
/// leaves the row queued and writes no frame; the acknowledgement then
/// forwards it.
#[tokio::test]
async fn forwarding_is_held_while_a_host_owes_a_fence_acknowledgement() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    let release = Arc::new(tokio::sync::Notify::new());
    worker_node.script().fence = Some(FenceReply::Hold(Arc::clone(&release)));

    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    let (first, _) = worker_node.next_frame().await?;
    assert_eq!(first, "lineage.fence");

    // A Human send to the worker host during the window.
    let command = cmd_id();
    let response: Value = ctx
        .send(&ctx.human, &worker, &command, "AFTER-ACK")
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(response["command"]["state"], json!("queued"));
    assert_eq!(response["command"]["forwarded"], json!(false));
    let row = ctx
        .hub
        .test_handle()
        .test_forward_command_online(&command)
        .await?;
    assert_eq!(row.state, "queued", "live=true does not bypass the window");
    assert!(!row.forwarded);
    assert!(worker_node.drain().is_empty(), "no frame was written");

    release.notify_one();
    let (method, params) = worker_node.next_frame().await?;
    assert_eq!(
        method, "instance.send",
        "the acknowledgement forwards the held row"
    );
    assert_eq!(params["commandId"], json!(command));
    Ok(())
}

/// §7.10 "Answer": a fence between authentication and `interaction.answer`
/// admission, and one between admission and send — the Node's interaction
/// stays pending (no frame).
#[tokio::test]
async fn a_fenced_answer_never_reaches_the_node() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let _worker = ctx.worker(&token, &mut worker_node).await?;
    let authenticated = initiator_of(&ctx, &seat)?;

    // (1) between admission and send: park after admission, then F.
    let (reached, release) = ctx.hub.test_handle().test_arm_fence_seam(
        "node-op-admitted",
        &ctx.worker_host,
        "op-answer-1",
    );
    let handle = ctx.hub.test_handle();
    let host = ctx.worker_host.clone();
    let initiator = authenticated.clone();
    let call = tokio::spawn(async move {
        handle
            .test_call_node_op(
                &host,
                "interaction.answer",
                "op-answer-1".into(),
                Some(initiator),
                None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), reached.notified())
        .await
        .context("barrier never reached")?;
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    release.send(()).ok();
    let error = call.await?.expect_err("refused after admission");
    assert!(error.to_string().contains("fenced"), "{error}");
    assert_eq!(
        ctx.op_outcome(&seat, "op-answer-1").await?.as_deref(),
        Some("cancelled-at-hub")
    );

    // (2) between authentication and admission: the authority was read
    // before F; admission after F refuses it.
    let error = ctx
        .hub
        .test_handle()
        .test_call_node_op(
            &ctx.worker_host,
            "interaction.answer",
            "op-answer-2".into(),
            Some(authenticated),
            None,
        )
        .await
        .expect_err("refused at admission");
    assert!(error.to_string().contains("fenced"), "{error}");
    assert!(
        worker_node
            .drain()
            .iter()
            .all(|(method, _)| method != "interaction.answer"),
        "the Node's interaction stays pending"
    );
    Ok(())
}

/// §7.10 "Worker provision and remove": a fence before either reaches the
/// Node — no frame, so no worktree is created or removed.
#[tokio::test]
async fn a_fence_before_worker_provision_or_remove_writes_nothing_to_the_node() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, _token) = ctx.seat(&mut seat_node).await?;
    let initiator = initiator_of(&ctx, &seat)?;
    // worker.provision: F commits between its admission and its send.
    let (reached, release) = ctx.hub.test_handle().test_arm_fence_seam(
        "node-op-admitted",
        &ctx.worker_host,
        "op-provision",
    );
    let handle = ctx.hub.test_handle();
    let host = ctx.worker_host.clone();
    let call_initiator = initiator.clone();
    let call = tokio::spawn(async move {
        handle
            .test_call_node_op(
                &host,
                "worker.provision",
                "op-provision".into(),
                Some(call_initiator),
                None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), reached.notified())
        .await
        .context("barrier never reached")?;
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    release.send(()).ok();
    let error = call.await?.expect_err("provision refused");
    assert!(error.to_string().contains("fenced"), "{error}");
    // worker.remove authenticated before F: once the host acknowledged the
    // fence (until then it is host-gated), refused at admission.
    ctx.wait_until(|| async {
        ctx.lineage(&seat)
            .await
            .ok()
            .and_then(|record| (record["fences"][0]["pendingHosts"] == json!([])).then_some(()))
    })
    .await?;
    let error = ctx
        .hub
        .test_handle()
        .test_call_node_op(
            &ctx.worker_host,
            "worker.remove",
            "op-remove".into(),
            Some(initiator),
            None,
        )
        .await
        .expect_err("remove refused");
    assert!(error.to_string().contains("fenced"), "{error}");
    assert_eq!(
        ctx.op_outcome(&seat, "op-provision").await?.as_deref(),
        Some("cancelled-at-hub")
    );
    assert!(
        worker_node
            .drain()
            .iter()
            .all(|(m, _)| m != "worker.provision" && m != "worker.remove"),
        "neither reached the Node, so no worktree was created or removed"
    );
    Ok(())
}

/// §7.10 "Node without fence support": a continuity lineage's command and
/// direct op to it are 409 `node-lacks-fence`; nothing is written.
#[tokio::test]
async fn a_node_without_fence_support_gets_409_node_lacks_fence() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut old_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, false).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    // Human work to the old Node is unchanged.
    let worker = ctx.worker(&token, &mut old_node).await?;
    let command = cmd_id();
    let refused = ctx.send(&token, &worker, &command, "TO-OLD-NODE").await?;
    assert_eq!(refused.status(), 409);
    let body: Value = refused.json().await?;
    assert_eq!(body["code"], json!("node-lacks-fence"), "{body}");
    assert_eq!(body["reason"], json!("node-lacks-fence"));
    assert_eq!(
        ctx.command_row(&command).await?.1.as_deref(),
        Some("rejected")
    );
    let error = ctx
        .hub
        .test_handle()
        .test_call_node_op(
            &ctx.worker_host,
            "worker.provision",
            "op-old".into(),
            Some(initiator_of(&ctx, &seat)?),
            None,
        )
        .await
        .expect_err("refused");
    assert!(error.to_string().contains("lineage fences"), "{error}");
    assert!(
        old_node
            .drain()
            .iter()
            .all(|(m, _)| m != "instance.send" && m != "worker.provision"),
        "nothing was written to the old Node"
    );
    Ok(())
}

/// §7.10 "Superseded interaction": the predecessor's pending interaction is
/// superseded by F; answering it is refused. Also the D-060 self-close
/// guard: a chapter with a pending interaction cannot close itself.
#[tokio::test]
async fn self_close_is_guarded_and_a_superseded_interaction_cannot_be_answered() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let (seat, token) = ctx.seat(&mut node).await?;
    let question = remuda_protocol::InteractionId::new()
        .as_id()
        .as_str()
        .to_owned();
    ctx.db()?.execute(
        "INSERT INTO interactions
            (id, instance_id, host_id, kind, state, blocking, payload_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'question', 'pending', 1, '{}', ?4, ?4)",
        rusqlite::params![question, seat, ctx.seat_host, "2026-10-31T00:00:00.000Z"],
    )?;
    let guarded = ctx.pause(&token, &seat).await?;
    assert_eq!(guarded.status(), 409);
    let body: Value = guarded.json().await?;
    assert_eq!(body["reason"], json!("pending-interaction"), "{body}");
    assert_eq!(ctx.lineage(&seat).await?["state"], json!("starting"));

    // The owner pauses instead: the question is superseded.
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    let (state, superseded_by): (String, Option<String>) = ctx.db()?.query_row(
        "SELECT state, superseded_by FROM interactions WHERE id = ?1",
        rusqlite::params![question],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(state, "superseded");
    assert_eq!(
        superseded_by.as_deref(),
        Some(seat.as_str()),
        "points at the paused lineage"
    );
    let answer = ctx
        .http
        .post(format!("{}/v1/interactions/{question}/answer", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"answer": {
            "kind": "plan-review", "optionId": "approve", "planRevision": "1",
            "planDigest": format!("sha256:{}", "a".repeat(64)), "feedback": null
        }}))
        .send()
        .await?;
    assert_eq!(answer.status(), 409);
    assert_eq!(answer.json::<Value>().await?["reason"], json!("superseded"));
    Ok(())
}

/// Self close without a pending interaction pauses as `paused{self}`.
#[tokio::test]
async fn a_self_close_pauses_as_self() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let (seat, token) = ctx.seat(&mut node).await?;
    ctx.pause(&token, &seat).await?.error_for_status()?;
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("paused"));
    assert_eq!(record["pausedBy"]["kind"], json!("self"));
    assert_eq!(record["pausedBy"]["instanceId"], json!(seat));
    Ok(())
}

/// §7.10 "Hub restart between F and fence delivery": the fence is durable
/// and delivered exactly once on the host's next hello.
#[tokio::test]
async fn a_fence_committed_before_a_hub_restart_is_delivered_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    let config = HubConfig::for_test(data.clone());
    let hub = spawn(config.clone()).await?;
    let human = hub.mint_device_token("phone").await?;
    let seat_host = HostId::new().as_id().as_str().to_owned();
    let worker_host = HostId::new().as_id().as_str().to_owned();
    let http = reqwest::Client::new();
    let mut seat_node = FakeNode::connect(&hub, &seat_host, true).await?;
    let worker_node = FakeNode::connect(&hub, &worker_host, true).await?;
    let worker_token = worker_node.node_token.clone().context("token")?;
    drop(worker_node);
    let base = format!("http://{}", hub.addr);
    let created: Value = http
        .post(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .json(
            &json!({"hostId": seat_host, "kind": "claude", "driver": "claude-sdk",
            "grants": ["address-owner"], "permissionMode": "manual",
            "restart": {"onProcessLoss": true, "maxPerHour": 3}, "prompt": "s"}),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let seat = created["instance"]["instanceId"]
        .as_str()
        .context("seat")?
        .to_owned();
    seat_node.next_work_frame().await?;
    http.post(format!("{base}/v1/instances/{seat}/commands"))
        .bearer_auth(&human)
        .json(&json!({"operation": "instance.close", "payload": {}}))
        .send()
        .await?
        .error_for_status()?;
    drop(seat_node);
    hub.shutdown().await;

    // Same data dir and bootstrap token: a Hub restart.
    let hub = spawn(config).await?;
    let mut worker_node =
        FakeNode::connect_with(&hub, &worker_host, &worker_token, true, json!({})).await?;
    let (first, fence) = worker_node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    assert_eq!(fence["lineageId"], json!(seat));
    let base = format!("http://{}", hub.addr);
    let human = hub.mint_device_token("phone-2").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let record: Value = http
                .get(format!("{base}/v1/lineages/{seat}"))
                .bearer_auth(&human)
                .send()
                .await?
                .json()
                .await?;
            let pending = record["fences"][0]["pendingHosts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if !pending.iter().any(|host| host == &json!(worker_host)) {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    drop(worker_node);
    let mut again =
        FakeNode::connect_with(&hub, &worker_host, &worker_token, true, json!({})).await?;
    // Sentinel: a Human create on the host. A pending fence would be
    // delivered (and acknowledged) before any forward to the host, so the
    // create frame arriving with no fence ahead of it proves none was re-sent.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let host: Value = http
                .get(format!("{base}/v1/hosts/{worker_host}"))
                .bearer_auth(&human)
                .send()
                .await?
                .json()
                .await?;
            if host["online"] == json!(true) {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    http.post(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .json(
            &json!({"hostId": worker_host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual", "prompt": "sentinel"}),
        )
        .send()
        .await?
        .error_for_status()?;
    let (method, _) = again.next_frame().await?;
    assert_eq!(
        method, "instance.create",
        "an acknowledged fence is never re-sent"
    );
    Ok(())
}

/// The seat's prompt to the worker was forwarded (accepted, unsettled) and
/// the worker host then partitioned; the owner pauses the seat. Returns
/// (ctx, seat, worker node token, command id) with the worker host offline.
async fn in_flight_then_partition(ctx: &Ctx) -> Result<(String, String, String, FakeNode)> {
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let worker_token = worker_node.node_token.clone().context("token")?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    let command = cmd_id();
    let sent: Value = ctx
        .send(&token, &worker, &command, "IN-FLIGHT")
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(sent["command"]["state"], json!("accepted"), "{sent}");
    worker_node.disconnect();
    ctx.wait_until(|| async {
        let host: Value = ctx
            .http
            .get(format!("{}/v1/hosts/{}", ctx.base(), ctx.worker_host))
            .bearer_auth(&ctx.human)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (host["online"] == json!(false)).then_some(())
    })
    .await?;
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    Ok((seat, worker_token, command, seat_node))
}

/// §7.10 "Prompt still queued when the Node reconnects", Hub half: the
/// fence is the first frame after the reconnect, and the Node's `cancelled`
/// settles the potentially-executed prompt as cancelled-by-node.
#[tokio::test]
async fn after_reconnect_the_fence_is_first_and_a_queued_prompt_resolves_cancelled_by_node()
-> Result<()> {
    let ctx = Ctx::boot().await?;
    let (seat, worker_token, command, _seat_node) = in_flight_then_partition(&ctx).await?;
    assert_eq!(
        ctx.op_outcome(&seat, &command).await?.as_deref(),
        Some("unknown")
    );
    let mut worker_node = FakeNode::connect_scripted(
        &ctx.hub,
        &ctx.worker_host,
        &worker_token,
        vec![vec![(command.clone(), "cancelled".into())]],
    )
    .await?;
    let (first, _) = worker_node.next_frame().await?;
    assert_eq!(
        first, "lineage.fence",
        "the fence is the first frame after reconnect"
    );
    let outcome = ctx
        .wait_until(|| async {
            let outcome = ctx.op_outcome(&seat, &command).await.ok().flatten()?;
            (outcome != "unknown").then_some(outcome)
        })
        .await?;
    assert_eq!(outcome, "cancelled-by-node");
    Ok(())
}

/// §7.10 "Turn ends during the partition", Hub half: the prompt shows
/// `unknown` until reconnect, then `ran`, never cancelled.
#[tokio::test]
async fn a_prompt_delivered_during_the_partition_is_unknown_until_reconnect_then_ran() -> Result<()>
{
    let ctx = Ctx::boot().await?;
    let (seat, worker_token, command, _seat_node) = in_flight_then_partition(&ctx).await?;
    assert_eq!(
        ctx.op_outcome(&seat, &command).await?.as_deref(),
        Some("unknown")
    );
    let mut worker_node = FakeNode::connect_scripted(
        &ctx.hub,
        &ctx.worker_host,
        &worker_token,
        vec![vec![(command.clone(), "ran".into())]],
    )
    .await?;
    let (first, _) = worker_node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    let outcome = ctx
        .wait_until(|| async {
            let outcome = ctx.op_outcome(&seat, &command).await.ok().flatten()?;
            (outcome != "unknown").then_some(outcome)
        })
        .await?;
    assert_eq!(outcome, "ran");
    Ok(())
}

/// §7.10 "Crash ambiguity", Hub half: the acknowledgement reports one op
/// `unknown` and omits another; the Hub keeps both unknown and never shows
/// either as not executed.
#[tokio::test]
async fn an_unknown_or_omitted_op_in_the_ack_stays_unknown() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    let (reported, omitted) = (cmd_id(), cmd_id());
    for command in [&reported, &omitted] {
        ctx.send(&token, &worker, command, "AMBIGUOUS")
            .await?
            .error_for_status()?;
    }
    worker_node
        .script()
        .fence_queue
        .push_back(vec![(reported.clone(), "unknown".into())]);
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    ctx.wait_until(|| async {
        ctx.lineage(&seat)
            .await
            .ok()
            .and_then(|record| (record["fences"][0]["pendingHosts"] == json!([])).then_some(()))
    })
    .await?;
    assert_eq!(
        ctx.op_outcome(&seat, &reported).await?.as_deref(),
        Some("unknown")
    );
    assert_eq!(
        ctx.op_outcome(&seat, &omitted).await?.as_deref(),
        Some("unknown")
    );
    for command in [&reported, &omitted] {
        let (state, outcome, _) = ctx.command_row(command).await?;
        assert_ne!(state, "settled", "nothing settles it as not executed");
        assert!(outcome.is_none());
    }
    Ok(())
}

/// D-060 addition: fence keys carry the generation — an op recorded under
/// one fence is settled only by that fence's own acknowledgement; a later
/// fence's ack naming it (a late result of an old chapter) changes nothing.
#[tokio::test]
async fn a_later_fences_ack_never_settles_an_op_recorded_under_an_earlier_fence() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut seat_node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    ctx.report_session(&seat_node, &seat).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    let old = cmd_id();
    ctx.send(&token, &worker, &old, "OLD-CHAPTER")
        .await?
        .error_for_status()?;
    // Fence 1 (pause): its ack says nothing about the old op.
    worker_node.script().fence_queue.push_back(Vec::new());
    ctx.pause(&ctx.human, &seat).await?.error_for_status()?;
    // Resume, then fence 2 (pause again): its ack names the old op.
    ctx.http
        .post(format!("{}/v1/instances/{seat}/resume", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?;
    let successor = ctx.lineage(&seat).await?["chapters"][1]["instanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    worker_node
        .script()
        .fence_queue
        .push_back(vec![(old.clone(), "cancelled".into())]);
    ctx.pause(&ctx.human, &successor)
        .await?
        .error_for_status()?;
    ctx.wait_until(|| async {
        let record = ctx.lineage(&seat).await.ok()?;
        let fences = record["fences"].as_array()?;
        (fences.len() == 2
            && fences
                .iter()
                .all(|fence| fence["pendingHosts"] == json!([])))
        .then_some(())
    })
    .await?;
    let record = ctx.lineage(&seat).await?;
    let first = &record["fences"][0];
    assert_eq!(first["fencedGeneration"], json!(1));
    let op = first["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["opId"] == json!(old))
        .expect("recorded under fence 1");
    assert_eq!(op["generation"], json!(1));
    assert_eq!(
        op["outcome"],
        json!("unknown"),
        "fence 2's ack cannot settle fence 1's op"
    );
    assert!(
        record["fences"][1]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op["opId"] != json!(old)),
        "an op belongs to exactly one fence"
    );
    Ok(())
}

/// §7.10 "Seat's Node changes epoch": the seat's Node reconnects with a new
/// epoch (its chapter gone) while the worker's Node holds the seat's
/// in-flight prompt. The restart decision's F (the C1 supervisor lands with
/// ma-restart; driven here through its test seam) records the prompt as
/// potentially executed, delivers the fence to the worker's Node (which
/// cancels it), and the successor's create reaches the seat's Node only
/// after that Node acknowledged the fence.
#[tokio::test]
async fn a_seat_node_epoch_change_fences_the_prompt_held_by_the_worker_node() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let mut seat_node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.seat_host,
        &enroll,
        true,
        json!({"nodeEpoch": "epoch_0190d8b2-6a1e-7c3b-9f21-4c5d6e7f8a10"}),
    )
    .await?;
    let seat_token = seat_node.node_token.clone().context("token")?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &ctx.worker_host, true).await?;
    let (seat, token) = ctx.seat(&mut seat_node).await?;
    ctx.report_session(&seat_node, &seat).await?;
    let worker = ctx.worker(&token, &mut worker_node).await?;
    let prompt = cmd_id();
    ctx.send(&token, &worker, &prompt, "HELD-BY-WORKER-NODE")
        .await?
        .error_for_status()?;
    let (method, _) = worker_node.next_work_frame().await?;
    assert_eq!(method, "instance.send");

    seat_node.disconnect();
    ctx.wait_until(|| async {
        let host: Value = ctx
            .http
            .get(format!("{}/v1/hosts/{}", ctx.base(), ctx.seat_host))
            .bearer_auth(&ctx.human)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (host["online"] == json!(false)).then_some(())
    })
    .await?;
    let mut seat_node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.seat_host,
        &seat_token,
        true,
        json!({"nodeEpoch": "epoch_0190d8b2-6a1e-7c3b-9f21-4c5d6e7f8a11", "instances": [], "instanceStoreFound": true}),
    )
    .await?;
    ctx.wait_until(|| async {
        let view: Value = ctx
            .http
            .get(format!("{}/v1/instances/{seat}", ctx.base()))
            .bearer_auth(&ctx.human)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        matches!(view["lifecycle"].as_str(), Some("exited" | "failed")).then_some(())
    })
    .await
    .context("the epoch change ends the seat's chapter")?;

    worker_node
        .script()
        .fence_queue
        .push_back(vec![(prompt.clone(), "cancelled".into())]);
    let successor = ctx
        .hub
        .test_handle()
        .test_restart_lineage(&seat)
        .await?
        .context("the restart decision commits")?;
    let outcome = ctx
        .wait_until(|| async {
            let outcome = ctx.op_outcome(&seat, &prompt).await.ok().flatten()?;
            (outcome != "unknown").then_some(outcome)
        })
        .await
        .with_context(|| "the worker Node's ack resolves the prompt")?;
    assert_eq!(outcome, "cancelled-by-node");
    let (first, fence) = seat_node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    assert_eq!(fence["liveGeneration"], json!("2"));
    let (second, create) = seat_node.next_frame().await?;
    assert_eq!(
        second, "instance.create",
        "the successor's create follows the ack"
    );
    assert_eq!(create["instanceId"], json!(successor));
    assert_eq!(create["lineage"], json!({"id": seat, "generation": "2"}));
    Ok(())
}

static RACE_HOOK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::const_new(()));

/// A parked hook point: `reached` flips when the hooked thread arrives; it
/// stays parked until `release`. Released gates stay open (a retry passes).
#[derive(Default)]
struct Gate {
    state: std::sync::Mutex<(bool, bool)>,
    cv: std::sync::Condvar,
}

impl Gate {
    fn arrive(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.cv.notify_all();
        while !state.1 {
            state = self.cv.wait(state).unwrap();
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.1 = true;
        self.cv.notify_all();
    }

    async fn reached(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.state.lock().unwrap().0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("gate never reached")
    }
}

/// §7.10 "Human close vs restart, both orders", driven by hooks: both
/// transactions read generation G; whichever commits first, the lineage ends
/// `paused{device}` with no live chapter and no admission from either.
async fn close_vs_restart(restart_first: bool) -> Result<()> {
    let _serial = RACE_HOOK.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.seat_host, true).await?;
    let (seat, _token) = ctx.seat(&mut node).await?;
    let original = initiator_of(&ctx, &seat)?;
    let close_gate = Arc::new(Gate::default());
    let restart_gate = Arc::new(Gate::default());
    {
        let (close_gate, restart_gate, lineage) = (
            Arc::clone(&close_gate),
            Arc::clone(&restart_gate),
            seat.clone(),
        );
        remuda_hub::lineage_test_support::set_hook(Some(Arc::new(move |point, id| {
            if id != lineage {
                return;
            }
            match point {
                "fence-read" => close_gate.arrive(),
                "restart-read" => restart_gate.arrive(),
                _ => {}
            }
        })));
    }
    let close = {
        let http = ctx.http.clone();
        let url = format!("{}/v1/instances/{seat}/commands", ctx.base());
        let human = ctx.human.clone();
        tokio::spawn(async move {
            http.post(url)
                .bearer_auth(human)
                .json(&json!({"operation": "instance.close", "payload": {}}))
                .send()
                .await
        })
    };
    close_gate.reached().await?;
    let handle = ctx.hub.test_handle();
    let lineage = seat.clone();
    let restart = tokio::spawn(async move { handle.test_restart_lineage(&lineage).await });
    restart_gate.reached().await?;
    if restart_first {
        restart_gate.release();
        let successor = restart.await??;
        assert!(successor.is_some(), "the restart committed first");
        close_gate.release();
        assert_eq!(close.await??.status(), 200);
    } else {
        close_gate.release();
        assert_eq!(close.await??.status(), 200);
        restart_gate.release();
        assert_eq!(restart.await??, None, "the restart's CAS sees the close");
    }
    remuda_hub::lineage_test_support::set_hook(None);

    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("paused"), "{record}");
    assert_eq!(record["pausedBy"]["kind"], json!("device"));
    let chapters = record["chapters"].as_array().cloned().unwrap_or_default();
    assert_eq!(chapters.len(), if restart_first { 2 } else { 1 });
    assert!(
        chapters
            .iter()
            .all(|chapter| chapter["fencedAt"].is_string()),
        "no live chapter: {record}"
    );
    // Nothing is admitted from either chapter.
    let mut initiators = vec![original];
    if restart_first {
        let successor = chapters[1]["instanceId"].as_str().unwrap();
        initiators.push(initiator_of(&ctx, successor)?);
    }
    for (index, initiator) in initiators.into_iter().enumerate() {
        let error = ctx
            .hub
            .test_handle()
            .test_call_node_op(
                &ctx.seat_host,
                "worker.provision",
                format!("op-race-{index}"),
                Some(initiator),
                None,
            )
            .await
            .expect_err("fenced");
        assert!(error.to_string().contains("fenced"), "{error}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_close_then_restart_ends_paused_by_the_device() -> Result<()> {
    close_vs_restart(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_then_human_close_ends_paused_by_the_device() -> Result<()> {
    close_vs_restart(true).await
}
