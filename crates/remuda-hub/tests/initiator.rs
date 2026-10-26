//! D-057 §7.1–§7.5 (ma-initiator): every Agent-initiated Hub mutation
//! carries the Hub-stamped initiator and authenticating device id, and the
//! commit-time check refuses work after a fence.
//!
//! The cases below fail on origin/main: there `queue_command` / gate insert /
//! task + project + worker writers ran no authority check, the answer path
//! called the Node before writing anything in the Hub, and no initiator was
//! stamped on commands or forwarded to Nodes.
//!
//! Everything drives the Hub over HTTP with Human + Agent (MCP) tokens and
//! one scripted fake Node that records every method it is asked to run.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

type AppendFrame = (String, Value);

const BRIEF: &str = "Do the tiny task.\nReply DONE <sha> or BLOCKED <reason>.\n";

/// A deterministic reply hold: after the fake records the RPC frame whose
/// `params.opId` matches, `arrived` fires and the canned reply is withheld
/// until the test calls [`ReplyHold::release`].
#[derive(Clone)]
struct ReplyHold {
    arrived: Arc<Notify>,
    released: Arc<AtomicBool>,
    release: Arc<Notify>,
}

impl ReplyHold {
    async fn wait_arrived(&self) {
        // Bounded: a missed arm/frame must fail the test, never hang it.
        tokio::time::timeout(Duration::from_secs(10), self.arrived.notified())
            .await
            .expect("held RPC frame never arrived within the bounded wait");
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

/// Deterministic control over the fake Node's handling of ONE `gate.run`
/// RPC: the canned result is withheld until `release()` (so the job stays
/// Running on the Hub while the test drives the `gate.event` finished
/// notification), and `arrived_rx` latches when the frame reaches the Node.
///
/// Arrival is a latched shared flag (its signal survives even if nobody
/// awaits yet — `Notify::notify_waiters()` alone would drop it), and every
/// wait below is bounded by a timeout.
#[derive(Clone)]
struct GateRunHook {
    /// Latch shared by the Node task and the test; set on the first frame.
    arrived: Arc<GateRunArrival>,
    released: Arc<AtomicBool>,
    release: Arc<Notify>,
    result: Arc<std::sync::Mutex<Value>>,
    /// When true the reply is a JSON-RPC ERROR (driving the Hub's real
    /// dispatch error callback), not a result.
    error: Arc<AtomicBool>,
}

/// A cloneable one-shot arrival latch: `signal()` stores a permit that a
/// later `wait()` observes (no publish-before-await race).
#[derive(Debug)]
struct GateRunArrival {
    notified: Notify,
    fired: AtomicBool,
}

impl GateRunHook {
    fn new(result: Value) -> Self {
        Self {
            arrived: Arc::new(GateRunArrival {
                notified: Notify::new(),
                fired: AtomicBool::new(false),
            }),
            released: Arc::new(AtomicBool::new(false)),
            release: Arc::new(Notify::new()),
            result: Arc::new(std::sync::Mutex::new(result)),
            error: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Make this hook answer gate.run with a JSON-RPC error frame, exercising
    /// the Hub dispatch callback's real error arm.
    fn reply_error(self: &Arc<Self>) {
        self.error.store(true, Ordering::SeqCst);
    }

    /// Latch arrival (idempotent: only the first frame fires it).
    fn mark_arrived(&self) {
        if self
            .arrived
            .fired
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.arrived.notified.notify_waiters();
        }
    }

    async fn wait_arrived(&self) {
        let timeout = Duration::from_secs(10);
        let notified = self.arrived.notified.notified();
        if self.arrived.fired.load(Ordering::Acquire) {
            return;
        }
        tokio::time::timeout(timeout, notified)
            .await
            .expect("gate.run frame never arrived");
    }

    #[allow(dead_code)]
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    /// Release the parked gate.run and WAIT (bounded) for the parked frame to
    /// actually proceed, so a release that misses the parked run fails the
    /// test fast instead of hanging the whole suite on its outer timeout
    /// (r14 item 8d). The fake Node's reply task reads this latch; here we
    /// verify the latch is observed and then the caller asserts on the job.
    async fn release_and_wait_proceeded(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
        // No waiter counter is exposed on Notify; the observable proof that the
        // parked frame proceeded is left to the caller (the job reaches the
        // expected state under its own bounded wait). This deadline only guards
        // the latch observation itself.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !self.released.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            self.released.load(Ordering::SeqCst),
            "gate.run release latch not observed within the deadline"
        );
    }

    fn result(&self) -> Value {
        self.result.lock().expect("gate result").clone()
    }

    fn set_result(&self, result: Value) {
        *self.result.lock().expect("gate result") = result;
    }
}

/// A verbatim node→Hub JSON-RPC frame (e.g. a `gate.event` notification).
type RawFrame = Value;

/// Service one inbound Hub RPC frame: track answer state, record the frame
/// for assertions, honor a reply hold, then answer with a canned result.
type FakeWs = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type FakeOut = std::sync::Arc<tokio::sync::Mutex<futures::stream::SplitSink<FakeWs, Message>>>;

async fn service_inbound(
    out: &FakeOut,
    frames: &mpsc::UnboundedSender<AppendFrame>,
    pending: &tokio::sync::Mutex<HashSet<String>>,
    holds: &std::sync::Mutex<HashMap<String, ReplyHold>>,
    gate_run: &tokio::sync::Mutex<Option<Arc<GateRunHook>>>,
    resolve: &tokio::sync::Mutex<Option<Value>>,
    frame: Value,
) {
    /// Send one JSON frame through the shared sink.
    macro_rules! send_frame {
        ($value:expr) => {{
            let mut sink = out.lock().await;
            sink.send(Message::Text($value.to_string().into())).await
        }};
    }
    // journal.append is node→hub: the Hub never sends it as an RPC request.
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        return;
    };
    if method == "journal.append" {
        return;
    }
    if method == "interaction.answer" {
        let id = frame["params"]["interactionId"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        pending.lock().await.remove(&id);
    }
    // Record BEFORE honoring the hold: arrival is observable as soon as the
    // frame is parsed, while the reply is still withheld.
    let _ = frames.send((method.to_owned(), frame["params"].clone()));
    // Take the hold out of the std mutex and DROP THE GUARD before any await
    // (a non-Send guard held across .await makes the node task !Send).
    let hold = frame
        .pointer("/params/opId")
        .and_then(Value::as_str)
        .and_then(|op_id| holds.lock().expect("holds").get(op_id).cloned());
    if let Some(hold) = hold {
        hold.arrived.notify_waiters();
        // r13 item 9: arm the notified() BEFORE checking the flag so a release
        // that lands between the load and the await cannot be missed (a missed
        // notify_waiters permit hung this loop forever).
        loop {
            let notified = hold.release.notified();
            if hold.released.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    }
    if frame.get("id").is_none() {
        return;
    }
    // A scripted, parkable gate.run: answer with the hook's canned verdict
    // (and withhold it until released). The hook is single-use (taken per
    // frame), so a requeue's attempt 2 can be scripted independently.
    if method == "gate.run"
        && let Some(hook) = gate_run.lock().await.take()
    {
        hook.mark_arrived();
        // Background reply so the read loop keeps servicing other
        // frames (a node->Hub gate.event) while this RPC is held.
        let out = out.clone();
        tokio::spawn(async move {
            // Arm notified() before the flag read so an immediate release is
            // not missed (r13 item 9).
            loop {
                let notified = hook.release.notified();
                if hook.released.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
            let rpc_id = frame["id"].clone();
            let mut sink = out.lock().await;
            let value = if hook.error.load(Ordering::SeqCst) {
                json!({"jsonrpc":"2.0","id":rpc_id,
                       "error":{"code":-32001,"message":"lane host RPC failed (test)"}})
            } else {
                json!({"jsonrpc":"2.0","id":rpc_id,"result":hook.result()})
            };
            let _ = sink.send(Message::Text(value.to_string().into())).await;
        });
        return;
    }
    let result = match method {
        // r10: answer the Hub-authority pre-run fence for this dispatch.
        "gate.authorize" => json!({
            "authorized": true,
            "generation": frame["params"]["generation"].clone(),
        }),
        // r15 item 4: scriptable read-only Landing probe reply.
        "gate.resolveLanding" => {
            if let Some(reply) = resolve.lock().await.take() {
                reply
            } else {
                json!({"jobId": frame["params"]["jobId"], "status": "unknown"})
            }
        }
        "worker.provision" => json!({
            "name": frame["params"]["name"].clone(),
            "branch": "wt/x/work",
            "worktreePath": "/tmp/remuda-wt/x",
            "targetDir": "/tmp/remuda-target/x"
        }),
        "worker.remove" => json!({
            "name":"x","worktreeRemoved":true,"targetRemoved":true,"reclaimedBytes":"4096"
        }),
        "instance.create" => json!({"accepted": true}),
        "interaction.answer" => json!({"accepted": true}),
        _ => json!({"accepted": true}),
    };
    let _ = send_frame!(json!({"jsonrpc":"2.0","id":frame["id"],"result":result}));
}

/// Fake Node: accepts every Hub RPC, records `(method, params)`, and lets the
/// test push journal events and hold individual RPC replies.
struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    frames: mpsc::UnboundedReceiver<AppendFrame>,
    appends: tokio::sync::mpsc::UnboundedSender<AppendFrame>,
    /// interactionId ids still pending at the fake Node (an
    /// `interaction.requested` journal event inserts; an answer would
    /// remove — the Hub never answers for the Node).
    pending: Arc<tokio::sync::Mutex<HashSet<String>>>,
    /// opId -> held reply (deterministic in-flight seam).
    holds: Arc<std::sync::Mutex<HashMap<String, ReplyHold>>>,
    /// Scripted parkable gate.run reply (one job at a time).
    gate_run: Arc<tokio::sync::Mutex<Option<Arc<GateRunHook>>>>,
    /// Scripted gate.resolveLanding reply (r15 item 4).
    resolve: Arc<tokio::sync::Mutex<Option<Value>>>,
    /// Verbatim node→Hub frames (e.g. a `gate.event` finished notification).
    raw: mpsc::UnboundedSender<RawFrame>,
}

/// Node credentials returned on first hello (D-018: re-enrollment of the same
/// host must present the persistent nodeToken).
#[derive(Clone)]
struct NodeCreds {
    host: String,
    token: String,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeNode {
    async fn connect_with_bearer(
        hub: &remuda_hub::RunningHub,
        host: &str,
        workspace: &str,
        bearer: &str,
    ) -> Result<(Self, String)> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse()?);
        let (node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut node = node;
        node.send(Message::Text(
            json!({
                "jsonrpc":"2.0","id":"hello","method":"runtime.hello",
                "params":{"hostId": host, "nodeVersion":"0.1.0-test", "host":{
                    "hostname":format!("{host}.local"),"maxInstances":8,
                    "resources":{"cpuCount":8,"cpuPct":5,"memPct":20,"loadAvg1":0.1,"diskFreeGb":120.0},
                    "cli":[{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr":{"version":"0.9.0","socket":"/tmp/fake.sock"},
                    "workspaces":[{"workspaceId":workspace,"hostId":host,"root":format!("/tmp/{workspace}")}],
                    "workspaceRevision":1
                }}
            })
            .to_string()
            .into(),
        ))
        .await?;
        let hello: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(8), node.next())
                .await
                .context("hello timeout")?
                .context("hello frame")??
                .into_text()?,
        )
        .context("hello json")?;
        // A reconnect hello is answered without a fresh nodeToken.
        anyhow::ensure!(hello.get("result").is_some(), "hello {hello}");

        let (frame_tx, frame_rx) = mpsc::unbounded_channel();
        let (append_tx, mut append_rx): (
            mpsc::UnboundedSender<AppendFrame>,
            mpsc::UnboundedReceiver<AppendFrame>,
        ) = mpsc::unbounded_channel();
        let pending = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        let pending_task = pending.clone();
        let answered_task = pending.clone();
        let holds: Arc<std::sync::Mutex<HashMap<String, ReplyHold>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let holds_task = holds.clone();
        let gate_run: Arc<tokio::sync::Mutex<Option<Arc<GateRunHook>>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let gate_run_task = gate_run.clone();
        let resolve: Arc<tokio::sync::Mutex<Option<Value>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let resolve_task = resolve.clone();
        let (raw_tx, mut raw_rx): (
            mpsc::UnboundedSender<RawFrame>,
            mpsc::UnboundedReceiver<RawFrame>,
        ) = mpsc::unbounded_channel();
        // Split the socket: a shared sink lets a parked gate.run reply wait
        // in a background task while the read loop keeps handling frames
        // (notably a node→Hub gate.event), like the real carrier.
        let (sink, mut node) = node.split();
        let out: FakeOut = std::sync::Arc::new(tokio::sync::Mutex::new(sink));
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(raw) = raw_rx.recv() => {
                        // A verbatim node→Hub frame (e.g. gate.event).
                        if out
                            .lock()
                            .await
                            .send(Message::Text(raw.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some((instance_id, event)) = append_rx.recv() => {
                        // Tracked on the append feed BEFORE the frame is
                        // sent: feed local to the test driver, so the Hub's
                        // append ack (and answer frames queued behind it)
                        // cannot delay it. The Hub derives the event kind
                        // at /kind with /payload fallbacks.
                        let append_kind = event
                            .get("kind")
                            .and_then(Value::as_str)
                            .or_else(|| event.pointer("/payload/kind").and_then(Value::as_str));
                        if matches!(
                            append_kind,
                            Some("interaction.requested" | "interactionRequested")
                        ) && let Some(id) = event
                            .pointer("/payload/interactionId")
                            .and_then(Value::as_str)
                        {
                            pending_task.lock().await.insert(id.to_owned());
                        }
                        let frame = json!({
                            "jsonrpc":"2.0","id":"append","method":"journal.append",
                            "params":{"instanceId":instance_id,"event":event}
                        });
                        if out
                            .lock()
                            .await
                            .send(Message::Text(frame.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        // Wait for the append ack matched BY ID. A Hub RPC
                        // that lands on the socket here is a REAL frame — it
                        // is recorded and answered, never swallowed as if it
                        // were the ack (that would hide a leaked forbidden
                        // frame and lose its response).
                        'ack: loop {
                            match node.next().await {
                                Some(Ok(Message::Text(text))) => {
                                    let Ok(fr) = serde_json::from_str::<Value>(&text) else {
                                        continue;
                                    };
                                    if fr.get("method").is_none()
                                        && fr.get("id").and_then(Value::as_str) == Some("append")
                                    {
                                        break 'ack;
                                    }
                                    service_inbound(
                                        &out,
                                        &frame_tx,
                                        &answered_task,
                                        &holds_task,
                                        &gate_run_task,
                                        &resolve_task,
                                        fr,
                                    )
                                    .await;
                                }
                                Some(Ok(Message::Close(_))) | None => return,
                                _ => {}
                            }
                        }
                    }
                    frame = node.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                                service_inbound(
                                    &out,
                                    &frame_tx,
                                    &answered_task,
                                    &holds_task,
                                    &gate_run_task,
                                    &resolve_task,
                                    frame,
                                )
                                .await;
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        Ok((
            Self {
                task,
                frames: frame_rx,
                appends: append_tx,
                pending: pending.clone(),
                holds,
                gate_run,
                resolve,
                raw: raw_tx,
            },
            hello["result"]["nodeToken"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
        ))
    }

    /// Fresh enrollment (new host): enrolls, returns the Node and the
    /// persistent credentials a later reconnect must present.
    async fn connect(
        hub: &remuda_hub::RunningHub,
        host: &str,
        workspace: &str,
    ) -> Result<(Self, NodeCreds)> {
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?;
        let (node, token) = Self::connect_with_bearer(hub, host, workspace, &enroll).await?;
        Ok((
            node,
            NodeCreds {
                host: host.to_owned(),
                token,
            },
        ))
    }

    /// Reconnect an enrolled host using its persistent nodeToken.
    async fn reconnect(
        hub: &remuda_hub::RunningHub,
        creds: &NodeCreds,
        workspace: &str,
    ) -> Result<Self> {
        Ok(
            Self::connect_with_bearer(hub, &creds.host, workspace, &creds.token)
                .await?
                .0,
        )
    }

    async fn next(&mut self) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(5), self.frames.recv())
            .await
            .expect("frame in time")
            .expect("sender alive")
    }

    /// Post a Human command sentinel, then assert it is the next frame the
    /// Node receives: nothing the call produced may queue ahead of it.
    /// Frames recorded before the assertion but emitted by the TEST ITSELF
    /// (e.g. the `interaction.list` reads of a Hub-side wait) are drained
    /// first; a frame in `forbidden` fails even there. With an empty
    /// forbidden set ANY pre-existing frame is a leak.
    async fn assert_sentinel_follows(&mut self, ctx: &Ctx, forbidden: &[&str]) {
        while let Ok((method, params)) = self.frames.try_recv() {
            if forbidden.contains(&method.as_str()) {
                panic!("forbidden frame reached the Node: {method} {params}");
            }
            if forbidden.is_empty() {
                panic!("a frame reached the Node before the sentinel: {method}");
            }
            // Non-empty forbidden set: this is a benign frame the test
            // itself emitted (e.g. interaction.list while waiting).
        }
        let (status, sentinel) = ctx
            .send(
                "POST",
                &format!("/v1/instances/{}/commands", ctx.instance),
                &ctx.human,
                None,
                Some(Ctx::send_body("instance.send")),
            )
            .await;
        assert_eq!(status, 200, "{sentinel}");
        let (method, params) = self.next().await;
        assert_eq!(
            method, "instance.send",
            "a frame reached the Node before the sentinel: {method} {params}"
        );
    }

    /// Strict sentinel: the call produced no frame at all.
    async fn assert_next_frame_is_sentinel(&mut self, ctx: &Ctx) {
        self.assert_sentinel_follows(ctx, &[]).await;
    }

    /// Install a deterministic reply hold (see [`ReplyHold`]) for the RPC
    /// frame carrying `opId` in its params.
    fn hold_reply_for(&self, op_id: String) -> ReplyHold {
        let hold = ReplyHold {
            arrived: Arc::new(Notify::new()),
            released: Arc::new(AtomicBool::new(false)),
            release: Arc::new(Notify::new()),
        };
        self.holds
            .lock()
            .expect("holds")
            .insert(op_id, hold.clone());
        hold
    }

    /// Push one journal event (synchronous channel send; the WS send and
    /// ack drain run in the connection task). The event is durably mirrored
    /// once the Hub ack comes back — tests that need that use
    /// [`FakeNode::is_pending`], which fills from the append feed before
    /// the frame is written.
    fn append(&self, instance_id: &str, event: Value) {
        self.appends
            .send((instance_id.to_owned(), event))
            .expect("append channel");
    }

    /// True while the Node still holds the interaction as pending (a
    /// `interaction.requested` was mirrored and no `interaction.answer`
    /// frame has arrived).
    async fn is_pending(&self, interaction_id: &str) -> bool {
        self.pending.lock().await.contains(interaction_id)
    }

    /// Script the NEXT `gate.run` RPC: park it at the Node with the given
    /// verdict until the hook is released. Returns the hook.
    async fn arm_gate_run(&self, result: Value) -> Arc<GateRunHook> {
        let hook = Arc::new(GateRunHook::new(result));
        *self.gate_run.lock().await = Some(hook.clone());
        hook
    }

    /// Send a verbatim node→Hub JSON-RPC frame (a `gate.event`
    /// notification).
    fn send_raw(&self, frame: Value) {
        self.raw.send(frame).expect("raw frame channel");
    }

    /// Script the next `gate.resolveLanding` reply (r15 item 4).
    async fn set_resolve_reply(&self, reply: Value) {
        *self.resolve.lock().await = Some(reply);
    }

    /// Pop the next inbound `(method, params)` the Hub asked the Node to run.
    async fn next_frame(&mut self) -> AppendFrame {
        self.frames.recv().await.expect("frame channel")
    }

    /// Drop the Node connection abruptly; the Hub marks its host offline.
    fn disconnect(&self) {
        self.task.abort();
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    db_path: std::path::PathBuf,
    creds: NodeCreds,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
    project: String,
    instance: String,
    agent: String,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn store(&self) -> &remuda_hub::store_test_support::Store {
        self.hub.store().expect("store")
    }

    fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }

    async fn boot() -> Result<(Ctx, FakeNode)> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().join("data");
        let hub = spawn(HubConfig::for_test(data_dir.clone())).await?;
        let human = hub.mint_device_token("initiator-phone").await?;
        let host = HostId::new().as_id().as_str().to_owned();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let (mut node, creds) = FakeNode::connect(&hub, &host, &workspace).await?;

        let http = reqwest::Client::new();
        let project: Value = http
            .post(format!("http://{}/v1/projects", hub.addr))
            .bearer_auth(&human)
            .json(&json!({
                "name":"initiator-project",
                "members":[{"hostId":host,"workspaceId":workspace,"role":"build"}],
                "hosts":[{"hostId":host,"maxInstances":8,"maxBuilding":4,"diskBudgetGb":10,
                          "portBlocks":["58700-58799"],"requires":[],"latencyClass":"remote"}]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let project_id = project["id"].as_str().unwrap().to_owned();

        // A coordinator chapter: dispatch + land grants, scoped to the
        // project and its host (the narrowed Human dispatch path checks
        // both project and host scope).
        let created: Value = http
            .post(format!("http://{}/v1/instances", hub.addr))
            .bearer_auth(&human)
            .json(&json!({
                "hostId": host, "kind":"claude","driver":"claude-print",
                "name":"coord","title":"Coordinator",
                "grants":["address-owner","dispatch","land"],
                "scope":{"projectIds":[project_id],"hostIds":[host]},
                "permissionMode":"manual",
                "prompt":"coord brief"
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let instance = created["instance"]["instanceId"]
            .as_str()
            .context("instance id")?
            .to_owned();
        let (method, create_params) = node.next().await;
        assert_eq!(method, "instance.create");

        let agent: Value = http
            .post(format!(
                "http://{}/v1/instances/{instance}/mcp-token",
                hub.addr
            ))
            .bearer_auth(&human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let agent = agent["token"].as_str().context("mcp token")?.to_owned();

        // The forwarded create already carries the human (None) initiator —
        // i.e. no initiator key at all.
        assert!(
            create_params.get("initiator").is_none(),
            "human create forwards no initiator: {create_params}"
        );

        Ok((
            Ctx {
                _dir: dir,
                db_path: data_dir.join("hub.sqlite"),
                creds,
                hub,
                http,
                human,
                host,
                workspace,
                project: project_id,
                instance,
                agent,
            },
            node,
        ))
    }

    /// Raw request; `narrow` adds x-remuda-instance-id (Human narrowing).
    async fn send(
        &self,
        method: &str,
        path: &str,
        token: &str,
        narrow: Option<&str>,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        let target = format!("{}{}", self.base(), path);
        let mut builder = self
            .http
            .request(method.parse().unwrap(), target.clone())
            .bearer_auth(token);
        if let Some(instance) = narrow {
            builder = builder.header("x-remuda-instance-id", instance);
        }
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder
            .send()
            .await
            .unwrap_or_else(|err| panic!("send {target}: {err}"));
        let status = response.status();
        let value = response.json().await.unwrap_or(json!(null));
        (status, value)
    }

    async fn agent_post(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.send("POST", path, &self.agent, None, Some(body)).await
    }

    async fn agent_patch(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.send("PATCH", path, &self.agent, None, Some(body))
            .await
    }

    async fn agent_delete(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.send("DELETE", path, &self.agent, None, Some(body))
            .await
    }

    async fn fence(&self) {
        self.store()
            .test_fence_instance(self.instance.clone())
            .await
            .expect("fence");
    }

    async fn command_rows(&self) -> Vec<Value> {
        let value: Value = self
            .http
            .get(format!(
                "{}/v1/instances/{}/commands",
                self.base(),
                self.instance
            ))
            .bearer_auth(&self.human)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        value["commands"].as_array().cloned().unwrap_or_default()
    }

    /// One interaction's Hub-projected state from the Hub's own list
    /// (`None` when the row is absent).
    async fn interaction_state(&self, interaction_id: &str) -> Option<String> {
        let list: Value = self
            .http
            .get(format!("{}/v1/interactions", self.base()))
            .bearer_auth(&self.human)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["interactionId"] == json!(interaction_id))
            .and_then(|row| row["state"].as_str())
            .map(str::to_string)
    }

    async fn create_task(&self, title: &str) -> Value {
        let (status, body) = self
            .agent_post(
                "/v1/tasks",
                json!({"projectId":self.project,"title":title,"intent":"do it"}),
            )
            .await;
        assert!(status.is_success(), "create_task {status} {body}");
        body
    }

    async fn task(&self, id: &str) -> Value {
        let (status, body) = self
            .send("GET", &format!("/v1/tasks/{id}"), &self.human, None, None)
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    fn send_body(operation: &str) -> Value {
        json!({"operation":operation,"payload":{"input":{"type":"prompt","text":"hi"}}})
    }

    /// The agent chapter's live initiator and its MCP device id, read
    /// straight from the Hub DB.
    fn agent_authority(&self) -> (remuda_protocol::Initiator, String) {
        let db = rusqlite::Connection::open(self.db_path()).unwrap();
        let (lineage_id, generation, device_id): (String, i64, String) = db
            .query_row(
                "SELECT i.lineage_id, COALESCE(l.generation, i.generation), d.id
                   FROM instances i
                   LEFT JOIN lineages l ON l.lineage_id = i.lineage_id
                   JOIN devices d ON d.instance_id = i.id
                  WHERE i.id = ?1
                  ORDER BY d.created_at DESC LIMIT 1",
                rusqlite::params![self.instance],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        (
            remuda_protocol::Initiator {
                instance_id: self.instance.clone(),
                lineage_id,
                generation,
            },
            device_id,
        )
    }

    /// Wait until the host catalog row reads `online: false` after a WS
    /// drop. Yields instead of sleeping: the writer applies the offline
    /// transition as soon as the close completes (5s watchdog only).
    async fn wait_host_offline(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let host: Value = self
                    .http
                    .get(format!("{}/v1/hosts/{}", self.base(), self.host))
                    .bearer_auth(&self.human)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if host["online"] == json!(false) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("host marked offline");
    }

    /// POST a task with a pool binding on the fake Node's workspace.
    async fn post_pool_task(&self, title: &str) -> (reqwest::StatusCode, Value) {
        self.agent_post(
            "/v1/tasks",
            json!({
                "projectId": self.project,
                "title": title,
                "intent": "bind and fence",
                "workspaceBinding": {
                    "mode": "pool",
                    "hostId": self.host,
                    "workspaceId": self.workspace,
                    "worktreeName": "p"
                }
            }),
        )
        .await
    }

    /// Task rows listed for the project.
    async fn task_list(&self) -> Vec<Value> {
        let (status, body) = self
            .send(
                "GET",
                &format!("/v1/tasks?project={}", self.project),
                &self.human,
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body["items"].as_array().cloned().unwrap_or_default()
    }

    /// Hub-side lease rows (state, holder) for the test's host/workspace.
    fn lease_rows(&self) -> Vec<(String, Option<String>)> {
        let db = rusqlite::Connection::open(self.db_path()).unwrap();
        let mut stmt = db
            .prepare(
                "SELECT state, holder_instance_id FROM worktree_leases
                  WHERE host_id = ?1 AND workspace_id = ?2",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![self.host, self.workspace], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// Configure one gate lane on the connected fake Node. Deliberately a
    /// separate call after the test is armed: with no lane configured the
    /// enqueue-time scheduler tick cannot dispatch, so the later explicit
    /// test_gate_tick is the unique dispatch attempt — no timing race.
    async fn configure_gate_lane(&self) {
        let (status, body) = self
            .send(
                "PATCH",
                &format!("/v1/projects/{}", self.project),
                &self.human,
                None,
                Some(json!({
                    "gate": {
                        "affected": true,
                        "web": "auto",
                        "landSerialization": "global-cas",
                        "lanes": [{
                            "id": "lane1",
                            "hostId": self.host,
                            "repoPath": "/tmp/lane1/repo",
                            "targetDir": "/tmp/lane1/target",
                            "ports": "58400-58409",
                            "env": {},
                            "lockPath": "/tmp/remuda-agents/e2e.lock",
                            "pwEndpoint": "ws://127.0.0.1:3177/"
                        }]
                    }
                })),
            )
            .await;
        assert_eq!(status, 200, "configure lane: {body}");
    }
}
fn assert_fenced(status: reqwest::StatusCode, body: &Value) {
    assert_eq!(status, 409, "expected 409 fenced, got {status}: {body}");
    assert_eq!(body["code"], json!("fenced"), "body: {body}");
}

// ── 1. Table-driven: every Agent-admitted writer route ─────────────────────

#[tokio::test]
async fn fenced_agent_is_refused_on_every_admitted_write_and_nothing_is_written() {
    let (ctx, _node) = Ctx::boot().await.unwrap();

    // A task that exists before the fence for the mutating routes.
    let task = ctx.create_task("pre-fence task").await;
    let task_id = task["id"].as_str().unwrap().to_owned();

    // A queued gate job for the cancel route (the project configures no
    // lanes, so the scheduler leaves it queued).
    let (status, queued_job) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/fenced/cancel","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{queued_job}");
    let queued_job_id = queued_job["id"].as_str().unwrap().to_owned();

    // The project's one member and no fleet instances exist yet.
    let members_before = 1i64;
    let instances_before = {
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.query_row("SELECT COUNT(*) FROM instances", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };

    let commands_before = ctx.command_rows().await.len();

    ctx.fence().await;

    // (label, status, body) cases; verification is per family below.
    // queue_command family: every command the agent can issue.
    let command_cases = ["instance.send", "instance.cancel", "instance.close"];
    for operation in command_cases {
        let (status, body) = ctx
            .agent_post(
                &format!("/v1/instances/{}/commands", ctx.instance),
                Ctx::send_body(operation),
            )
            .await;
        assert_fenced(status, &body);
    }

    // instance create (insert_instance_delegated + create command).
    let (status, body) = ctx
        .agent_post(
            "/v1/instances",
            json!({"hostId":ctx.host,"kind":"claude","driver":"claude-print",
                   "permissionMode":"manual","prompt":"child"}),
        )
        .await;
    assert_fenced(status, &body);

    // gate job insert after the fence is refused.
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/fenced/verify","mode":"verify"}),
        )
        .await;
    assert_fenced(status, &body);

    // gate cancel is Agent-admitted: the fenced canceller must not move a
    // queued job or a running one.
    let (status, body) = ctx
        .agent_post(
            &format!(
                "/v1/projects/{}/gate/jobs/{queued_job_id}/cancel",
                ctx.project
            ),
            json!({}),
        )
        .await;
    assert_fenced(status, &body);

    // project PATCH/members.
    let (status, body) = ctx
        .agent_patch(
            &format!("/v1/projects/{}", ctx.project),
            json!({"name":"renamed-after-fence"}),
        )
        .await;
    assert_fenced(status, &body);
    // POST /members is idempotent for the project's own member: it 409s
    // "member already exists" before the authority check on origin/main,
    // so after F it must be the 409 fenced refusal instead.
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/members", ctx.project),
            json!({"hostId":ctx.host,"workspaceId":ctx.workspace,"role":"build"}),
        )
        .await;
    assert_fenced(status, &body);
    // DELETE /members removes an existing member — nothing written after F.
    let (status, body) = ctx
        .agent_delete(
            &format!("/v1/projects/{}/members", ctx.project),
            json!({"hostId":ctx.host,"workspaceId":ctx.workspace}),
        )
        .await;
    assert_fenced(status, &body);

    // fleet create: every spawn runs check_initiator in-job.
    let (status, body) = ctx
        .agent_post(
            "/v1/fleet/instances",
            json!({
                "hosts":[ctx.host],
                "kind":"claude","driver":"claude-print",
                "spec":{"permissionMode":"manual","prompt":"fleet child"}
            }),
        )
        .await;
    assert_fenced(status, &body);

    // project members + fleet create refused with no new member/instance row.
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/members", ctx.project),
            json!({"hostId":ctx.host,"workspaceId":ctx.workspace,"role":"build"}),
        )
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_delete(
            &format!("/v1/projects/{}/members", ctx.project),
            json!({"hostId":ctx.host,"workspaceId":ctx.workspace}),
        )
        .await;
    assert_fenced(status, &body);

    // fleet create: every spawn runs check_initiator in-job.
    let (status, body) = ctx
        .agent_post(
            "/v1/fleet/instances",
            json!({
                "hosts":[ctx.host],
                "kind":"claude","driver":"claude-print",
                "spec":{"permissionMode":"manual","prompt":"fleet child"}
            }),
        )
        .await;
    assert_fenced(status, &body);

    // task writers, including land.
    let (status, body) = ctx
        .agent_post(
            "/v1/tasks",
            json!({"projectId":ctx.project,"title":"post-fence","intent":"x"}),
        )
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/tasks/{task_id}/split"),
            json!({"title":"post-fence child","intent":"x"}),
        )
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_patch(&format!("/v1/tasks/{task_id}"), json!({"state":"running"}))
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_post(&format!("/v1/tasks/{task_id}/archive"), json!({}))
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/tasks/{task_id}/land"),
            json!({"sha":"0123456789abcdef0123456789abcdef01234567"}),
        )
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/tasks/{task_id}/own"),
            json!({"paths":["src/new.rs"]}),
        )
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_delete(
            &format!("/v1/tasks/{task_id}/own"),
            json!({"paths":["src/new.rs"]}),
        )
        .await;
    assert_fenced(status, &body);
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/tasks/{task_id}/placements"),
            json!({"kind":"unplace"}),
        )
        .await;
    assert_fenced(status, &body);

    // fleet broadcast: per-target failure carries the fenced reason.
    let (status, body) = ctx
        .agent_post(
            "/v1/fleet/broadcast",
            json!({"operation":"instance.send","hosts":[ctx.host],"payload":{"text":"hi"}}),
        )
        .await;
    assert_eq!(
        status, 200,
        "broadcast aggregates per-target results: {body}"
    );
    assert_eq!(body["failed"], json!(1), "{body}");
    assert!(
        body["results"][0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("fenced"),
        "{body}"
    );

    // Nothing written: no new command rows; task unchanged; project unchanged;
    // no gate job queued; no project member or fleet instance added.
    assert_eq!(ctx.command_rows().await.len(), commands_before);
    let project_after: Value = ctx
        .http
        .get(format!("{}/v1/projects/{}", ctx.base(), ctx.project))
        .bearer_auth(&ctx.human)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        project_after["members"].as_array().map(std::vec::Vec::len),
        Some(members_before as usize),
        "member add/remove wrote after F: {project_after}"
    );
    let instances_after = {
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.query_row("SELECT COUNT(*) FROM instances", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };
    assert_eq!(
        instances_after, instances_before,
        "fleet create wrote after F"
    );
    let task_after = ctx.task(&task_id).await;
    assert_eq!(
        task_after["title"], task["title"],
        "task mutated despite fence"
    );
    let project: Value = ctx
        .http
        .get(format!("{}/v1/projects/{}", ctx.base(), ctx.project))
        .bearer_auth(&ctx.human)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        project["name"],
        json!("initiator-project"),
        "project renamed"
    );
    let gates: Value = ctx
        .http
        .get(format!("{}/v1/projects/{}/gate", ctx.base(), ctx.project))
        .bearer_auth(&ctx.human)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let jobs = gates["items"].as_array().cloned().unwrap_or_default();
    // Only the pre-fence job survives; the post-fence enqueue wrote nothing
    // and the fenced cancel left the row `queued`.
    assert_eq!(jobs.len(), 1, "post-fence gate writes leaked: {gates}");
    assert_eq!(jobs[0]["id"], json!(queued_job_id), "{gates}");
    assert_eq!(jobs[0]["state"], json!("queued"), "{gates}");
}

// ── 2. Authenticated before F ─────────────────────────────────────────────

#[tokio::test]
async fn request_that_passes_authentication_before_fence_is_refused_at_commit() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let rows_before = ctx.command_rows().await.len();

    ctx.store()
        .test_arm_fence_before_queue(ctx.instance.clone());

    let (status, body) = ctx
        .agent_post(
            &format!("/v1/instances/{}/commands", ctx.instance),
            Ctx::send_body("instance.send"),
        )
        .await;
    assert_fenced(status, &body);
    assert_eq!(
        ctx.command_rows().await.len(),
        rows_before,
        "fenced request wrote a row"
    );
}

// ── 3. Device-specific: the MCP token row is the one re-checked ───────────

#[tokio::test]
async fn request_authenticated_with_mcp_token_is_refused_when_only_that_device_row_is_deleted() {
    let (ctx, _node) = Ctx::boot().await.unwrap();

    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let mcp_device: String = db
        .query_row(
            "SELECT id FROM devices WHERE instance_id = ?1 ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![ctx.instance],
            |row| row.get(0),
        )
        .unwrap();
    let launch_device: String = db
        .query_row(
            "SELECT id FROM devices WHERE instance_id = ?1 ORDER BY created_at ASC LIMIT 1",
            rusqlite::params![ctx.instance],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(
        mcp_device, launch_device,
        "instance has launch + MCP devices"
    );
    drop(db);

    // The hook deletes the MCP device row INSIDE the queue writer job, like F
    // removing a predecessor credential between authentication and commit.
    ctx.store().test_arm_delete_device_before_queue(mcp_device);

    let (status, body) = ctx
        .agent_post(
            &format!("/v1/instances/{}/commands", ctx.instance),
            Ctx::send_body("instance.send"),
        )
        .await;
    assert_fenced(status, &body);

    // The launch credential still exists: only the authenticating row gates.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let launch_exists: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM devices WHERE id = ?1",
            rusqlite::params![launch_device],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(launch_exists, 1);
}

// ── 4. Same-id replay of a held row ───────────────────────────────────────

#[tokio::test]
async fn same_id_replay_of_a_held_row_after_fence_is_refused_and_never_reaches_node() -> Result<()>
{
    let (ctx, node) = Ctx::boot().await?;

    let command_id = remuda_protocol::CommandId::new()
        .as_id()
        .as_str()
        .to_owned();
    let mut body = Ctx::send_body("instance.send");
    body["commandId"] = json!(command_id);

    // Take the host offline: the first POST queues a HELD row (offline host,
    // no forward intent, no frame). Wait until the Hub has marked it offline.
    node.disconnect();
    ctx.wait_host_offline().await;
    let (status, first) = ctx
        .agent_post(
            &format!("/v1/instances/{}/commands", ctx.instance),
            body.clone(),
        )
        .await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(
        first["command"]["forwarded"],
        json!(false),
        "the row must be held, not forwarded: {first}"
    );

    // Fence while the row is still queued.
    ctx.fence().await;

    // The host comes back; the same-id D-055 replay must now refuse at the
    // mark_forward_intent writer job (re-checking the stamp ON THE ROW)
    // before any frame reaches the Node.
    let _node = FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace).await?;
    let (status, replay) = ctx
        .agent_post(&format!("/v1/instances/{}/commands", ctx.instance), body)
        .await;
    assert_fenced(status, &replay);

    Ok(())
}

// ── 5. interaction.answer: admission row exists BEFORE the Node call ──────

#[tokio::test]
async fn fenced_interaction_answer_never_reaches_the_node_and_stays_pending() {
    // D-051 routes questions to Agent parents; the gating is feature-switched.
    let _d051 = InitiatorD051Guard::on();
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // The Node reports a pending question owned by the coordinator chapter.
    let interaction_id = format!("int_{}", uuid::Uuid::now_v7());
    node.append(
        &ctx.instance,
        json!({
            "kind":"interaction.requested",
            "payload":{
                "kind":"question",
                "interactionId": interaction_id
            }
        }),
    );

    // Deterministic wait on the HUB's own state: the journal append projects
    // the requested interaction into the Hub's list (pending) once its ack is
    // matched by id — no fixed window, and nothing about this wait depends on
    // the fake's local bookkeeping.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if ctx.interaction_state(&interaction_id).await.as_deref() == Some("pending") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Hub projects the requested interaction pending");
    // The fake Node holds it pending too (same append).
    assert!(
        node.is_pending(&interaction_id).await,
        "requested interaction pending at the Node"
    );

    // Fence lands inside the answer's admission writer job.
    ctx.store()
        .test_arm_fence_before_authority_check(ctx.instance.clone());

    let (status, body) = ctx
        .agent_post(
            &format!("/v1/interactions/{interaction_id}/answer"),
            json!({
                "commandId": remuda_protocol::CommandId::new().as_id().as_str(),
                "answer": {"kind": "question", "answers": {}}
            }),
        )
        .await;
    assert_fenced(status, &body);

    // The fake Node never saw an answer. The Hub-state wait above legitimately
    // emitted interaction.list reads; those drain away, and no
    // interaction.answer may be among them — the sentinel is then the next
    // frame.
    node.assert_sentinel_follows(&ctx, &["interaction.answer"])
        .await;

    // R8c: the fake Node's OWN pending state is unchanged — the answer frame
    // that removes it never arrived (the Hub list assertion below alone
    // cannot prove the Node side).
    assert!(
        node.is_pending(&interaction_id).await,
        "the Node stopped holding the interaction pending despite the refused answer"
    );

    // The Hub never mirrored an answer: still pending in the list.
    assert_eq!(
        ctx.interaction_state(&interaction_id).await.as_deref(),
        Some("pending"),
        "answer committed before the Node"
    );
}

/// Process-global D-051 feature switch, on for the test's lifetime.
struct InitiatorD051Guard;

impl InitiatorD051Guard {
    fn on() -> Self {
        remuda_hub::delegated_decisions_test_support::set_global_override(Some(true));
        Self
    }
}

impl Drop for InitiatorD051Guard {
    fn drop(&mut self) {
        remuda_hub::delegated_decisions_test_support::set_global_override(None);
    }
}

// ── 6. Real scheduler tick: a fence or a revoked device cancels the claim ─

/// Enqueue a verify job while no lane exists, then run one scenario against
/// a real `tick()` once the lane is configured.
async fn queued_job(ctx: &Ctx) -> String {
    let (status, job) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/claim","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{job}");
    job["id"].as_str().unwrap().to_owned()
}

/// Enqueue a LAND verify job; a base-moved verdict genuinely requeues only a
/// land (a verify has no CAS to lose), so generation-gate requeue tests use it.
async fn queued_land_job(ctx: &Ctx) -> String {
    let (status, job) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/requeue-land","mode":"land"}),
        )
        .await;
    assert_eq!(status, 200, "{job}");
    job["id"].as_str().unwrap().to_owned()
}

/// Drive a GENUINE base-moved requeue through the production path (no raw
/// SQL): after attempt 1 is claimed, deliver its base-moved verdict through
/// the real `apply_result` writer (Running → Queued), then claim attempt 2.
///
/// r17 item 2: like the production Node's CAS-loss branch, the verdict
/// PRESERVES the exact granted merge pinned at verify (read from the live row)
/// — every Landing transition, including a requeue, must name that merge.
async fn genuine_requeue_to_attempt_two(ctx: &Ctx, job_id: &str) {
    let granted_merge = ctx
        .hub
        .test_get_gate_job(job_id)
        .await
        .unwrap()
        .and_then(|doc| {
            doc.get("mergeSha")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    ctx.hub
        .test_apply_gate_result(
            job_id,
            json!({
                "jobId": job_id,
                "generation": 1,
                "status": "base-moved",
                "mergeSha": granted_merge,
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567",
            }),
        )
        .await
        .unwrap();
    let claimed = ctx
        .hub
        .test_claim_gate_job(job_id, "lane1", &ctx.host)
        .await
        .unwrap();
    assert!(claimed.is_some(), "scheduler must claim requeued attempt 2");
    let doc = ctx.hub.test_get_gate_job(job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "reclaim: {doc}");
    assert_eq!(doc["attempts"], json!(2), "reclaim: {doc}");
}

#[tokio::test]
async fn gate_job_fenced_before_the_tick_is_canceled_and_never_dispatched() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let job_id = queued_job(&ctx).await;

    // F lands before the claim (whole instance fenced).
    ctx.fence().await;
    ctx.configure_gate_lane().await;
    ctx.hub.test_gate_tick().await;

    let doc = ctx
        .hub
        .test_get_gate_job(&job_id)
        .await
        .unwrap()
        .expect("job still exists");
    assert_eq!(doc["state"], json!("canceled"), "{doc}");
    assert_eq!(doc["reason"], json!("fenced"), "{doc}");
    node.assert_next_frame_is_sentinel(&ctx).await;
}

#[tokio::test]
async fn gate_job_whose_authenticating_device_was_deleted_is_canceled_at_claim() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let job_id = queued_job(&ctx).await;

    // Only the MCP device row is revoked, as F deletes predecessor
    // credentials: the instance is not marked fenced and the launch
    // credential still exists.
    let mcp_device = {
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        let id: String = db
            .query_row(
                "SELECT id FROM devices WHERE instance_id = ?1 ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![ctx.instance],
                |row| row.get(0),
            )
            .unwrap();
        db.execute("DELETE FROM devices WHERE id = ?1", rusqlite::params![id])
            .unwrap();
        id
    };
    ctx.configure_gate_lane().await;
    ctx.hub.test_gate_tick().await;

    let doc = ctx
        .hub
        .test_get_gate_job(&job_id)
        .await
        .unwrap()
        .expect("job still exists");
    assert_eq!(doc["state"], json!("canceled"), "{doc}");
    assert_eq!(doc["reason"], json!("fenced"), "{doc}");
    assert!(
        doc.get("initiatorDeviceId").is_none(),
        "device id stays off the API doc: {doc}"
    );
    node.assert_next_frame_is_sentinel(&ctx).await;

    // The launch credential is untouched.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let launch_exists: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM devices
              WHERE instance_id = ?1 AND id != ?2",
            rusqlite::params![ctx.instance, mcp_device],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(launch_exists, 1, "launch device row should survive");
}

// ── 6b. Gate cancel is checked in the mutating writer (both states) ───────

#[tokio::test]
async fn fenced_cancel_of_a_queued_or_running_gate_job_is_refused_before_the_node() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // Enqueue both jobs and pin each row to a running lane before the fence.
    let mut job_ids = Vec::new();
    for mode in ["verify", "land"] {
        let (status, job) = ctx
            .agent_post(
                &format!("/v1/projects/{}/gate", ctx.project),
                json!({"branch": format!("wt/fenced/cancel-{mode}"), "mode": mode}),
            )
            .await;
        assert_eq!(status, 200, "{job}");
        let job_id = job["id"].as_str().unwrap().to_owned();

        // Claim the row the way the scheduler would: running, pinned to the
        // connected fake Node's lane. The row never goes through dispatch()
        // (no gate.run is sent) — only the cancel route under test can emit.
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.execute(
            "UPDATE gate_jobs
                SET state = 'running', lane_id = 'lane1',
                    doc_json = json_set(
                        json_set(json_set(doc_json,
                            '$.state', 'running'),
                            '$.laneId', 'lane1'),
                        '$.hostId', ?2)
              WHERE id = ?1",
            rusqlite::params![job_id, ctx.host],
        )
        .unwrap();
        drop(db);
        job_ids.push(job_id);
    }

    ctx.fence().await;

    for job_id in &job_ids {
        let (status, body) = ctx
            .agent_post(
                &format!("/v1/projects/{}/gate/jobs/{job_id}/cancel", ctx.project),
                json!({}),
            )
            .await;
        assert_fenced(status, &body);

        // The row is untouched and no gate.cancel reached the Node.
        let doc = ctx.hub.test_get_gate_job(job_id).await.unwrap().unwrap();
        assert_eq!(doc["state"], json!("running"), "{doc}");
        assert!(
            doc["cancelRequestedAt"].is_null(),
            "cancel stamped despite fence: {doc}"
        );
    }
    // Deterministic no-frame proof: a sentinel Human command is the next
    // frame the Node receives (twice — no gate.cancel frames queue ahead).
    let (status, sentinel) = ctx
        .send(
            "POST",
            &format!("/v1/instances/{}/commands", ctx.instance),
            &ctx.human,
            None,
            Some(Ctx::send_body("instance.send")),
        )
        .await;
    assert_eq!(status, 200, "{sentinel}");
    let (method, _) = node.next().await;
    assert_eq!(method, "instance.send", "a gate.cancel frame leaked");
}

// ── 6b'. Cancel branches on the state observed INSIDE the writer ──────────

/// Race A: the handler pre-reads `queued`, but the 1 s scheduler tick claims
/// the job (running, gate.run spawned) before the cancel writer job reads it.
/// The cancel must follow the RUNNING branch — stamp canceling AND deliver
/// gate.cancel — instead of 409-ing over a transition it itself committed.
#[tokio::test]
async fn cancel_of_a_job_claimed_between_preread_and_writer_still_sends_gate_cancel() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let job_id = queued_job(&ctx).await;

    // No lane is configured: the real 1 s scheduler tick therefore cannot
    // dispatch this job (previously the test configured the lane and could
    // race the tick). The seam itself pins laneId/hostId on the row, which is
    // all the cancel handler needs to address gate.cancel.
    //
    // Deterministic seam: inside the cancel writer job, before its read, the
    // scheduler claim commits (queued -> running, pinned to the lane) —
    // exactly what the 1 s tick can do between the handler's pre-read and
    // the writer. No gate.run is sent in this test; the seam models the row
    // transition, and gate.cancel below is the frame under test.
    ctx.store().test_arm_gate_cancel_race(
        remuda_hub::store_test_support::TestGateCancelRace::Claimed {
            lane_id: "lane1".to_string(),
            host_id: ctx.host.clone(),
        },
    );

    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate/jobs/{job_id}/cancel", ctx.project),
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "a claimed job must accept the cancel: {body}");
    assert_eq!(body["state"], json!("canceling"), "{body}");

    // The writer-observed Running branch OWES gate.cancel: it is the very
    // next frame, carrying the jobId and lane.
    let (method, params) = node.next().await;
    assert_eq!(method, "gate.cancel", "gate.cancel was not delivered");
    assert_eq!(params["jobId"], json!(job_id), "{params}");
    assert_eq!(params["laneId"], json!("lane1"), "{params}");

    // The row really committed canceling (not terminal canceled, which would
    // skip the frame after the grace expiry).
    let doc = ctx
        .hub
        .test_get_gate_job(&job_id)
        .await
        .unwrap()
        .expect("job exists");
    assert_eq!(doc["state"], json!("canceling"), "{doc}");
    assert!(
        !doc["cancelRequestedAt"].is_null(),
        "cancel stamp missing: {doc}"
    );
}

/// Race B: the handler pre-reads `running`, but the dispatch finishes the
/// job before the cancel writer reads it. The terminal row is left untouched,
/// the answer is 409, and no gate.cancel frame reaches the Node.
#[tokio::test]
async fn cancel_of_a_job_finished_between_preread_and_writer_is_409_and_sends_nothing() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let job_id = queued_job(&ctx).await;

    // Pin the job running the way the scheduler does.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    db.execute(
        "UPDATE gate_jobs
            SET state = 'running', lane_id = 'lane1',
                doc_json = json_set(json_set(json_set(doc_json,
                    '$.state', 'running'),
                    '$.laneId', 'lane1'),
                    '$.hostId', ?2)
          WHERE id = ?1",
        rusqlite::params![job_id, ctx.host],
    )
    .unwrap();
    drop(db);

    // The dispatch passes before the writer's read.
    ctx.store().test_arm_gate_cancel_race(
        remuda_hub::store_test_support::TestGateCancelRace::Finished { state: "passed" },
    );

    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate/jobs/{job_id}/cancel", ctx.project),
            json!({}),
        )
        .await;
    assert_eq!(status, 409, "a terminal job cannot be canceled: {body}");

    // Untouched terminal row.
    let doc = ctx
        .hub
        .test_get_gate_job(&job_id)
        .await
        .unwrap()
        .expect("job exists");
    assert_eq!(doc["state"], json!("passed"), "{doc}");
    assert!(doc["cancelRequestedAt"].is_null(), "{doc}");

    // No gate.cancel frame: the sentinel arrives first.
    node.assert_next_frame_is_sentinel(&ctx).await;
}

/// A cancel request committing strictly between `apply_result`'s pre-read
/// and its writer job must not be overwritten. The stale pre-read saw
/// Running; inside the writer the row is Canceling. A land `base-moved`
/// verdict used to rewrite that Canceling row back to Queued (re-queue for
/// another verify+land), so the job ran again and could land although the
/// cancel caller already held a 200.
#[tokio::test]
async fn a_cancel_between_a_result_preread_and_its_writer_keeps_the_row_canceling() {
    let (ctx, _node) = Ctx::boot().await.unwrap();

    // A LAND job, queued then claimed to Running exactly like a tick claim
    // (no lane is configured, so the real tick never dispatches it).
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/cancel-vs-result","mode":"land"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued land job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    // Deterministic seam: the cancel commits Running -> Canceling INSIDE
    // the result's mutate_gate_job writer job, before the doc loads.
    ctx.store()
        .test_arm_cancel_before_gate_mutate(job_id.clone());

    // The real apply_result path: pre-read sees Running, the seam then
    // commits the cancel, and the writer re-checks the live state.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "status": "base-moved",
                "generation": 1,
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567"
            }),
        )
        .await
        .unwrap();

    let doc = ctx
        .hub
        .test_get_gate_job(&job_id)
        .await
        .unwrap()
        .expect("job exists");
    assert_ne!(
        doc["state"],
        json!("queued"),
        "the stale pre-read rewrote the Canceling row to Queued; the job would run and land again: {doc}"
    );
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "the writer-observed Canceling state must settle the base-moved verdict as canceled: {doc}"
    );
    assert!(
        !doc["cancelRequestedAt"].is_null(),
        "the cancel stamp must survive the result: {doc}"
    );
}

/// Shared setup: a LAND job claimed to Running and then moved to Canceling
/// exactly as the cancel writer leaves it (state, lane/host pin, stamp).
///
/// The cancel stamp is NOW, not a backdated 2020 value: the Hub's 1 s tick
/// runs `finish_expired_cancels` concurrently, and an expired stamp would
/// let that loop settle the row before the result's pre-read — the old
/// pre-read early-return would then make these tests pass with the writer
/// guard reverted. With an unexpired stamp ONLY the armed seam can finish
/// the row inside the result writer.
async fn canceling_land_job(ctx: &Ctx) -> String {
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/duplicate-verdict","mode":"land"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued land job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");
    // The cancel commit: Running -> Canceling with the pin and a FRESH stamp
    // the real cancel writer records (no gate.cancel frame is needed here).
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    db.execute(
        "UPDATE gate_jobs
            SET state = 'canceling',
                doc_json = json_set(
                    json_set(
                        json_set(doc_json, '$.state', 'canceling'),
                        '$.cancelRequestedAt', strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    '$.laneId', 'lane1'),
                revision = revision + 1,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE id = ?1",
        rusqlite::params![job_id],
    )
    .unwrap();
    drop(db);
    job_id
}

/// A land job in Canceling receives its base-moved verdict TWICE (the Node's
/// `finished` event AND the gate.run reply, through independent queues). Both
/// calls pre-read Canceling; the first writer commits Canceled. The second
/// writer must stand down — it used to fall through and requeue the
/// terminal row, so the next tick dispatched gate.run again and a
/// pushFrom-lane land could move main after the cancel was already 200.
#[tokio::test]
async fn a_duplicate_base_moved_verdict_after_the_row_is_canceled_never_requeues() {
    use remuda_hub::store_test_support::TestGateFinishBeforeWrite;

    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let job_id = canceling_land_job(&ctx).await;

    // Deterministic view of the SECOND writer: its pre-read observed
    // Canceling (the row is Canceling now), and before its writer loads the
    // doc the FIRST duplicate arrival commits Canceled — the seam reproduces
    // that terminal write inside this writer job.
    ctx.store().test_arm_finish_before_gate_mutate(
        job_id.clone(),
        TestGateFinishBeforeWrite::DuplicateVerdict,
    );
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "status": "base-moved",
                "generation": 1,
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567"
            }),
        )
        .await
        .unwrap();

    // The seam MUST have fired inside the result writer (if the pre-read had
    // early-returned — e.g. the 1 s grace finisher settled the row first —
    // the seam would still be armed and this assertion fails).
    assert!(
        ctx.store().take_test_finish_before_gate_mutate().is_none(),
        "the finish seam did not fire inside the result writer"
    );

    // A genuinely later third delivery now pre-reads Canceled as well and
    // must be ignored without any seam.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "status": "base-moved",
                "generation": 1,
                "currentMainSha": "fedcba9876543210fedcba9876543210fedcba98"
            }),
        )
        .await
        .unwrap();

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "the duplicate verdict requeued the terminal land job: {doc}"
    );
    assert!(
        !doc["finishedAt"].is_null(),
        "the terminal finish stamp must survive the duplicate: {doc}"
    );

    // Configure the lane ONLY now (the row is terminal): with an eligible
    // lane the follow-up tick really would dispatch a wrongly-Queued row,
    // which makes the no-gate.run assertion below meaningful.
    ctx.configure_gate_lane().await;
    // The next scheduler tick must not re-dispatch: the row stays Canceled
    // and no gate.run frame reaches the Node.
    ctx.hub.test_gate_tick().await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "a tick requeued/dispatched the canceled job: {doc}"
    );
    node.assert_next_frame_is_sentinel(&ctx).await;
}

/// Same window, different interloper: the cancel-grace finisher
/// (finish_expired_cancels, which runs on every tick) commits the
/// Canceling -> Canceled write strictly between a base-moved result's
/// pre-read and its writer. The result writer must leave the row Canceled.
#[tokio::test]
async fn a_grace_expiry_finish_between_a_result_preread_and_its_writer_keeps_the_row_canceled() {
    use remuda_hub::store_test_support::TestGateFinishBeforeWrite;

    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let job_id = canceling_land_job(&ctx).await;

    // The seam stands in for finish_expired_cancels's writer committing the
    // exact terminal transition (state canceled + finishedAt + the grace
    // error/reason) before this result's writer loads its doc.
    ctx.store()
        .test_arm_finish_before_gate_mutate(job_id.clone(), TestGateFinishBeforeWrite::GraceExpiry);
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "status": "base-moved",
                "generation": 1,
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567"
            }),
        )
        .await
        .unwrap();

    // The grace seam MUST have fired inside the result writer (a fresh
    // cancelRequestedAt keeps the real 1 s grace loop from settling the row
    // first, so only the seam can have produced the terminal write here).
    assert!(
        ctx.store().take_test_finish_before_gate_mutate().is_none(),
        "the grace finish seam did not fire inside the result writer"
    );

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "the grace finish was overwritten by the stale base-moved verdict: {doc}"
    );
    assert_eq!(
        doc["reason"],
        json!("canceled; run did not stop in time"),
        "the grace finisher's terminal evidence must survive: {doc}"
    );
    // Eligible lane only once the row is terminal, so a tick genuinely would
    // dispatch if the row were wrongly requeued.
    ctx.configure_gate_lane().await;
    ctx.hub.test_gate_tick().await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "tick revived the job: {doc}"
    );
    node.assert_next_frame_is_sentinel(&ctx).await;
}

/// A FAILED verdict (with bounded runLog) is delivered twice — the Node's
/// `finished` event AND the gate.run reply. Both handlers pre-read Running;
/// the first persists its log and commits Failed; the second persists its
/// copy BEFORE loading the (now Failed) row, then stops at the terminal
/// guard. The second persist must not delete the object the first writer
/// referenced: the job's logObjectId has to resolve (by object id and by
/// job id) after both arrivals. Regression for r8 item 1.
#[tokio::test]
async fn a_duplicate_failed_verdict_keeps_the_referenced_gate_log_object() {
    use remuda_hub::store_test_support::TestGateFinishBeforeWrite;

    let (ctx, _node) = Ctx::boot().await.unwrap();

    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/duplicate-failed-log","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued verify job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    let failed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "failed",
        "failedStep": "cargo-test",
        "error": "cargo-test failed: 1 test",
        "reason": "1 test failed",
        "runLog": {
            "step": "cargo-test",
            "kind": "failed",
            "attempts": 1,
            "headline": "cargo-test failed",
            "summary": ["test it_breaks ... FAILED"],
            "tail": ["failures:", "    it_breaks"],
            "linesSeen": 2
        }
    });

    // First arrival: persists the log under its content-addressed object id
    // and commits Failed with logObjectId set.
    ctx.hub
        .test_apply_gate_result(&job_id, failed_result.clone())
        .await
        .unwrap();
    let first = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(first["state"], json!("failed"), "{first}");
    let object_id = first["logObjectId"]
        .as_str()
        .expect("logObjectId after first arrival")
        .to_owned();
    assert!(object_id.starts_with("obj_"), "{object_id}");

    // Deterministic view of the SECOND handler: its pre-read saw Running
    // (both reads happened before the first writer committed), so put the
    // row back exactly as it was at that read. Its own writer then finds the
    // first arrival's terminal Failed write (the seam) and must stand down —
    // but its persist runs first, exactly the reported ordering.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    db.execute(
        "UPDATE gate_jobs
            SET state = 'running',
                doc_json = json_set(doc_json, '$.state', 'running')
          WHERE id = ?1",
        rusqlite::params![job_id],
    )
    .unwrap();
    drop(db);
    ctx.store().test_arm_finish_before_gate_mutate(
        job_id.clone(),
        TestGateFinishBeforeWrite::DuplicateFailed,
    );
    ctx.hub
        .test_apply_gate_result(&job_id, failed_result)
        .await
        .unwrap();

    // The seam MUST have fired inside the second writer (if the pre-read had
    // early-returned on a terminal row, the seam would still be armed).
    assert!(
        ctx.store().take_test_finish_before_gate_mutate().is_none(),
        "the finish seam was not consumed: the second result's writer did not run"
    );

    // The job still points at a retrievable object: by object id AND by job
    // id, same object, same bytes.
    let (status, by_object) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "referenced log object 404: {by_object}");
    assert_eq!(by_object["objectId"], json!(object_id), "{by_object}");
    assert_eq!(
        by_object["log"]["headline"],
        json!("cargo-test failed"),
        "{by_object}"
    );

    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "latest log by job id 404: {by_job}");
    assert_eq!(
        by_job["objectId"],
        json!(object_id),
        "job-id lookup resolves to a different object: {by_job}"
    );

    // The terminal row is Failed with the reference intact.
    let final_doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(final_doc["state"], json!("failed"), "{final_doc}");
    assert_eq!(
        final_doc["logObjectId"],
        json!(object_id),
        "the duplicate rewrote/dropped the reference: {final_doc}"
    );
}

/// Plant a claimed Running gate job that references a LEGACY random log
/// object id A (the pre-r8 shape) and the A row itself. Returns (job id,
/// object id, original log bytes).
async fn running_job_with_legacy_log(ctx: &Ctx) -> (String, String, Vec<u8>) {
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/legacy-log","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claim");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    // A pre-upgrade random object id (NOT the deterministic obj_<job>).
    let legacy_id = "obj_00112233445566778899aabbccddeeff00";
    let legacy_log = json!({
        "step": "cargo-test",
        "kind": "failed",
        "attempts": 1,
        "headline": "the OLD evidence",
        "summary": ["old failure"],
        "tail": ["old tail"],
        "linesSeen": 1
    });
    let legacy_bytes = serde_json::to_vec(&legacy_log).unwrap();
    // Digest only keys the same-bytes short-circuit; a distinct fixed value
    // is enough (the new result carries different bytes).
    let digest = "legacy-log-digest-v1";
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    db.execute(
        "INSERT INTO gate_log_objects
            (id, job_id, project_id, bytes, byte_len, digest,
             created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'dev', '2020-01-01T00:00:00.000Z',
                 '2099-01-01T00:00:00.000Z')",
        rusqlite::params![
            legacy_id,
            job_id,
            ctx.project,
            legacy_bytes,
            legacy_bytes.len() as i64,
            digest as &str,
        ],
    )
    .unwrap();
    db.execute(
        "UPDATE gate_jobs
            SET doc_json = json_set(doc_json, '$.logObjectId', ?2)
          WHERE id = ?1",
        rusqlite::params![job_id, legacy_id],
    )
    .unwrap();
    drop(db);
    (job_id, legacy_id.to_owned(), legacy_bytes)
}

fn failed_result_with_log(job_id: &str, headline: &str) -> Value {
    json!({
        "jobId": job_id,
        "generation": 1,
        "status": "failed",
        "failedStep": "cargo-test",
        "error": "cargo-test failed: 1 test",
        "reason": "1 test failed",
        "runLog": {
            "step": "cargo-test",
            "kind": "failed",
            "attempts": 1,
            "headline": headline,
            "summary": ["test it_new ... FAILED"],
            "tail": ["failures:", "    it_new"],
            "linesSeen": 2
        }
    })
}

/// A failed gate verdict carrying bounded evidence, for the real two-path
/// (finished event + gate.run reply) test. Same bytes on both paths so the
/// second persist is a digest no-op.
fn failed_gate_result(job_id: &str, _branch: &str) -> Value {
    json!({
        "jobId": job_id,
        "generation": 1,
        "status": "failed",
        "failedStep": "cargo-test",
        "error": "cargo-test failed: 1 test",
        "reason": "1 test failed",
        "runLog": {
            "step": "cargo-test",
            "kind": "failed",
            "attempts": 1,
            "headline": "the two-path evidence",
            "summary": ["test it_two_paths ... FAILED"],
            "tail": ["failures:", "    it_two_paths"],
            "linesSeen": 2
        }
    })
}

/// r19 item 4: a pre-upgrade job references a LEGACY object id holding OLD
/// bytes. A later runLog gets its OWN immutable content-addressed object and
/// can never mutate the legacy row. Here a competing terminal write (the
/// duplicate's first arrival) makes THIS writer stop at the guard after it
/// persisted, so the job keeps the legacy reference, which keeps serving the
/// OLD bytes by object id and by job id.
#[tokio::test]
async fn a_late_runlog_never_overwrites_the_jobs_legacy_log_object() {
    use remuda_hub::store_test_support::TestGateFinishBeforeWrite;

    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (job_id, legacy_id, _old_bytes) = running_job_with_legacy_log(&ctx).await;

    // The second handler's perspective: its pre-read saw Running and the
    // first duplicate arrival's Failed write lands inside its writer after
    // the persist. Its OWN object is inserted immutably, and the writer then
    // stands down without rebinding, so the legacy reference is untouched.
    ctx.store().test_arm_finish_before_gate_mutate(
        job_id.clone(),
        TestGateFinishBeforeWrite::DuplicateFailed,
    );
    ctx.hub
        .test_apply_gate_result(&job_id, failed_result_with_log(&job_id, "the NEW evidence"))
        .await
        .unwrap();

    // The seam's terminal write preserved the legacy reference; the late
    // writer did not rebind it.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("failed"), "{doc}");
    assert_eq!(
        doc["logObjectId"],
        json!(legacy_id),
        "the legacy reference stays bound through the interleave: {doc}"
    );

    let (status, by_object) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{legacy_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "legacy object 404: {by_object}");
    assert_eq!(by_object["objectId"], json!(legacy_id), "{by_object}");
    assert_eq!(
        by_object["log"]["headline"],
        json!("the OLD evidence"),
        "the immutable legacy object must keep the OLD bytes: {by_object}"
    );
    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "by-job lookup 404: {by_job}");
    assert_eq!(
        by_job["objectId"],
        json!(legacy_id),
        "by-job lookup resolves elsewhere: {by_job}"
    );
    assert_eq!(
        by_job["log"]["headline"],
        json!("the OLD evidence"),
        "by-job lookup still serves the immutable OLD evidence: {by_job}"
    );
}

/// A failed INSERT in the immutable log writer must leave the previously
/// referenced object served (nothing is deleted). r20 item 7 / r21 item 4:
/// the definitive settlement also STANDS DOWN — the job stays Running and
/// keeps its prior reference because the new result supplied a log that was
/// never persisted; it may not fail the job under another result's old bytes.
/// r22 item 1 / R9 removed the Hub-side retry scheduler: the stand-down does
/// NOT consume the verdict. After storage recovers the job is STILL Running
/// (nothing retries on the Hub); the existing delivery path then RE-SUPPLIES
/// the exact verdict, which settles Failed bound to its OWN object, exactly
/// once — a duplicate redelivery keeps that same object.
#[tokio::test]
async fn a_failed_log_insert_keeps_the_previous_object_served() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (job_id, legacy_id, old_bytes) = running_job_with_legacy_log(&ctx).await;

    // Make the writer fail EVERY insert until recovered (sticky), so the
    // r21-item-5 automatic retries stay deferred while the state is asserted.
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, failed_result_with_log(&job_id, "never stored"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // r20 item 7: the definitive Failed settlement DEFERS — the job is still
    // Running, not failed under the legacy log.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "settlement must stand down: {doc}"
    );
    assert_eq!(
        doc["logObjectId"],
        json!(legacy_id),
        "no new reference is bound by a failed persist: {doc}"
    );

    // The transaction rolled back: the OLD object is still served, by object
    // id and by job id.
    let (status, by_object) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{legacy_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(
        status, 200,
        "previous object lost after failed insert: {by_object}"
    );
    assert_eq!(
        by_object["log"]["headline"],
        json!("the OLD evidence"),
        "the rollback did not preserve the old bytes: {by_object}"
    );
    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "by-job lookup lost: {by_job}");
    assert_eq!(by_job["objectId"], json!(legacy_id), "{by_job}");
    // The old headline already proves the original bytes survived.
    let _ = old_bytes;

    // r22 R9: storage recovers, but the Hub runs NO persistence retry of its
    // own — the unconsumed verdict must be RE-SUPPLIED by the existing
    // delivery path (Node redelivery / reconciliation / operator). Wait well
    // past where the old r21 scheduler retried (75 ms …) to prove it is gone.
    ctx.store().test_clear_gate_log_insert_fault();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "the Hub must not self-retry persistence: the job awaits redelivery: {doc}"
    );

    // The existing delivery path re-supplies the exact verdict. It now
    // persists its OWN object and commits Failed bound to it (never to the
    // legacy bytes).
    let redelivered = failed_result_with_log(&job_id, "never stored");
    ctx.hub
        .test_apply_gate_result(&job_id, redelivered.clone())
        .await
        .unwrap();
    let settled = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(settled["state"], json!("failed"), "{settled}");
    let new_id = settled["logObjectId"]
        .as_str()
        .expect("the redelivery binds its persisted object")
        .to_owned();
    assert_ne!(
        new_id, legacy_id,
        "the settled evidence is not the legacy log"
    );

    // A further redelivery of the same verdict settles NO second time: same
    // terminal state, same bound object.
    ctx.hub
        .test_apply_gate_result(&job_id, redelivered)
        .await
        .unwrap();
    let again = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(again["state"], json!("failed"), "{again}");
    assert_eq!(
        again["logObjectId"],
        json!(new_id),
        "the duplicate redelivery must not rebind the evidence: {again}"
    );

    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{new_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    assert_eq!(
        served["log"]["headline"],
        json!("never stored"),
        "the Failed settlement is bound to the re-supplied result's own bytes: {served}"
    );
}

/// r21 item 4: a PASSED verify verdict that SUPPLIES a run log whose insert
/// fails must stand down exactly like the Failed case — the job stays Running
/// and keeps the prior reference; Passed must never commit with missing
/// evidence or under another result's old bytes (successful Node runs supply
/// such logs with keepLogs). r22 item 1 / R9: with the Hub-side retry
/// scheduler gone, the job remains Running even AFTER storage recovers until
/// the existing delivery path re-supplies the verdict; the redelivery then
/// settles Passed bound to its OWN object, exactly once.
#[tokio::test]
async fn a_failed_log_insert_makes_a_passed_verification_stand_down() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (job_id, legacy_id, _old_bytes) = running_job_with_legacy_log(&ctx).await;

    let passed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "passed",
        "runLog": {
            "step": "*",
            "kind": "kept",
            "attempts": 1,
            "headline": "green but never stored",
            "summary": ["cargo test ... ok"],
            "tail": [],
            "capturedLines": 1,
            "truncated": false
        }
    });

    // Storage fails EVERY insert until the test recovers it: the arrival and
    // all r21-item-5 retries keep deferring, so the standing-down state is
    // observable rather than healing on the first retry.
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, passed_result.clone())
        .await
        .unwrap();
    // Let several automatic retries land against the still-broken store.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Passed still DEFERS: Running, legacy reference untouched, no Passed row.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "a passed verdict whose evidence did not persist must stand down: {doc}"
    );
    assert_eq!(
        doc["logObjectId"],
        json!(legacy_id),
        "the failed persist must not rebind the reference: {doc}"
    );
    // While deferred, the by-job lookup keeps serving the OLD bytes.
    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{by_job}");
    assert_eq!(by_job["objectId"], json!(legacy_id), "{by_job}");

    // Storage recovers, but R9 gives the Hub no self-retry: the job must stay
    // Running until the verdict is RE-SUPPLIED by the existing delivery path.
    ctx.store().test_clear_gate_log_insert_fault();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "Passed must not settle via a Hub-side retry after recovery: {doc}"
    );

    // Redelivery of the same passed verdict persists its own object and
    // settles Passed bound to it; a second redelivery keeps that object.
    ctx.hub
        .test_apply_gate_result(&job_id, passed_result.clone())
        .await
        .unwrap();
    let settled = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(settled["state"], json!("passed"), "{settled}");
    let new_id = settled["logObjectId"]
        .as_str()
        .expect("the Passed job binds its persisted object")
        .to_owned();
    assert_ne!(
        new_id, legacy_id,
        "the settled evidence is not the legacy log"
    );
    ctx.hub
        .test_apply_gate_result(&job_id, passed_result)
        .await
        .unwrap();
    let again = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        again["logObjectId"],
        json!(new_id),
        "the duplicate redelivery must not rebind the evidence: {again}"
    );
    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{new_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    assert_eq!(
        served["log"]["headline"],
        json!("green but never stored"),
        "the Passed settlement binds the re-supplied result's own bytes: {served}"
    );
}

/// r22 item 1 / R9: a supplied run log LARGER than the evidence cap is NOT a
/// transient failure — a redelivery would carry the same oversized bytes
/// forever, so deferral could never heal. The Hub persists a truncated copy
/// carrying an explicit marker, binds the settlement to the digest of EXACTLY
/// the bytes stored, and settles normally (no stand-down, no redelivery
/// required).
#[tokio::test]
async fn an_oversized_run_log_is_persisted_truncated_and_settles_normally() {
    // Mirrors GATE_LOG_MAX_BYTES in gatequeue.rs.
    const EVIDENCE_CAP: usize = 512 * 1024;

    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/oversized-log","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued verify job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    // One tail line well over the cap on its own.
    let huge_line = "x".repeat(EVIDENCE_CAP + 128 * 1024);
    let failed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "failed",
        "failedStep": "cargo-test",
        "error": "cargo-test failed: 1 test",
        "reason": "1 test failed",
        "runLog": {
            "step": "cargo-test",
            "kind": "failed",
            "attempts": 1,
            "headline": "too much evidence",
            "summary": ["test it_huge ... FAILED"],
            "tail": [huge_line],
            "capturedLines": 1,
            "truncated": false
        }
    });

    ctx.hub
        .test_apply_gate_result(&job_id, failed_result)
        .await
        .unwrap();

    // The oversized verdict SETTLES (it is not deferred) and binds an object.
    let settled = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        settled["state"],
        json!("failed"),
        "an oversized log is truncated, not deferred: {settled}"
    );
    let object_id = settled["logObjectId"]
        .as_str()
        .expect("the truncated settlement still binds its stored object")
        .to_owned();
    assert!(object_id.starts_with("obj_"), "{object_id}");

    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    let log = &served["log"];
    assert_eq!(
        log["headline"],
        json!("too much evidence"),
        "the truncation keeps the log's identity fields: {log}"
    );
    assert_eq!(
        log["truncated"],
        json!(true),
        "the stored object must carry the explicit truncated flag: {log}"
    );
    let tail = log["tail"].as_array().expect("tail array: {log}");
    assert!(
        tail.iter()
            .any(|line| line.as_str().is_some_and(|text| text.contains("truncated"))),
        "the stored tail must carry an explicit truncation marker: {log}"
    );
    // The object is exactly the bounded bytes the digest was computed over.
    let stored_bytes = serde_json::to_vec(log).unwrap();
    assert!(
        stored_bytes.len() <= EVIDENCE_CAP,
        "the persisted object is {} bytes, over the {EVIDENCE_CAP} cap",
        stored_bytes.len()
    );
    assert_ne!(
        stored_bytes.len(),
        0,
        "the truncated object still carries the bounded log"
    );
}

/// r23 item 2: an oversized SCALAR metadata field — a 600 KiB `step` name and
/// a 600 KiB `kind` — used to bypass the evidence cap entirely: the r22
/// truncator only halved tail/summary and trimmed the headline, leaving step
/// and kind unbounded, so the serialized object stayed over 512 KiB (a debug
/// panic in dev, an oversized persisted row in release). EVERY variable
/// length field is now bounded and the final serialized-byte cap is enforced
/// at runtime, while the truncation marker survives.
#[tokio::test]
async fn an_oversized_step_and_kind_are_bounded_and_the_object_stays_under_the_cap() {
    const EVIDENCE_CAP: usize = 512 * 1024;
    const STEP_CAP: usize = 128;
    const KIND_CAP: usize = 32;

    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/oversized-metadata","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claims to running");

    // Small tail/summary/headline; the ONLY oversized fields are step+kind.
    // In the old code this serialized to ~1.2 MiB and never shrank.
    let huge_step = "s".repeat(600 * 1024);
    let huge_kind = "k".repeat(600 * 1024);
    let failed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "failed",
        "failedStep": huge_step,
        "error": "one test failed",
        "reason": "one test failed",
        "runLog": {
            "step": huge_step,
            "kind": huge_kind,
            "attempts": 1,
            "headline": "normal headline",
            "summary": ["test it_one ... FAILED"],
            "tail": ["failures:", "    it_one"],
            "capturedLines": 2,
            "truncated": false
        }
    });

    ctx.hub
        .test_apply_gate_result(&job_id, failed_result)
        .await
        .unwrap();

    let settled = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(settled["state"], json!("failed"), "{settled}");
    let object_id = settled["logObjectId"]
        .as_str()
        .expect("the bounded settlement binds its stored object")
        .to_owned();

    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    let log = &served["log"];
    let step = log["step"].as_str().expect("a string step: {log}");
    let kind = log["kind"].as_str().expect("a string kind: {log}");
    assert!(
        step.len() <= STEP_CAP && step.ends_with('…'),
        "the 600 KiB step is bounded and marked cut, got {} bytes",
        step.len()
    );
    assert!(
        kind.len() <= KIND_CAP && kind.ends_with('…'),
        "the 600 KiB kind is bounded and marked cut, got {} bytes",
        kind.len()
    );
    assert_eq!(log["truncated"], json!(true), "{log}");
    let tail = log["tail"].as_array().expect("tail array: {log}");
    assert!(
        tail.iter()
            .any(|line| line.as_str().is_some_and(|text| text.contains("truncated"))),
        "the truncation marker survives: {log}"
    );
    let stored_bytes = serde_json::to_vec(log).unwrap();
    assert!(
        stored_bytes.len() <= EVIDENCE_CAP,
        "the persisted object is {} bytes, over the {EVIDENCE_CAP} cap",
        stored_bytes.len()
    );

    // The JOB ROW also has its free-form fields bounded (failedStep).
    let failed_step = settled["failedStep"].as_str().expect("{settled}");
    assert!(
        failed_step.len() <= STEP_CAP,
        "the job row failedStep is bounded, got {} bytes",
        failed_step.len()
    );
}
/// r22 item 2: a Canceling job's Canceled transition is result-driven too. A
/// completed Passed-with-keepLogs verdict racing the cancel must only commit
/// Canceled ("verified but not landed") bound to its OWN persisted log; if
/// its insert fails the Canceled settlement DEFERS (the row stays Canceling
/// with the prior reference), and redelivery after recovery settles it once.
#[tokio::test]
async fn a_canceling_passed_verdict_with_missing_evidence_defers_its_cancel_settlement() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/cancel-passed-evidence","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued verify job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");
    // The operator cancel commits while the green result is still in flight.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    db.execute(
        "UPDATE gate_jobs
            SET state = 'canceling',
                doc_json = json_set(doc_json, '$.state', 'canceling'),
                revision = revision + 1,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE id = ?1",
        rusqlite::params![job_id],
    )
    .unwrap();
    drop(db);

    let passed = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "passed",
        "runLog": {
            "step": "*",
            "kind": "kept",
            "attempts": 1,
            "headline": "green when the cancel landed",
            "summary": ["cargo test ... ok"],
            "tail": ["Finished", "test result: ok"],
            "capturedLines": 2,
            "truncated": false
        }
    });

    // The green verdict's evidence cannot persist: the result-driven Canceled
    // transition must stand down rather than record "verified but not landed"
    // with no bound evidence.
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, passed.clone())
        .await
        .unwrap();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceling"),
        "the cancel settlement must defer without its own persisted log: {doc}"
    );
    assert!(
        doc["logObjectId"].is_null(),
        "a failed persist must not bind evidence during deferral: {doc}"
    );

    // Storage recovers and the existing path re-supplies the verdict: it now
    // settles Canceled exactly once, bound to its own kept log.
    ctx.store().test_clear_gate_log_insert_fault();
    ctx.hub
        .test_apply_gate_result(&job_id, passed.clone())
        .await
        .unwrap();
    let settled = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(settled["state"], json!("canceled"), "{settled}");
    assert_eq!(
        settled["reason"],
        json!("canceled; verified but not landed"),
        "{settled}"
    );
    let object_id = settled["logObjectId"]
        .as_str()
        .expect("the Canceled settlement binds the kept log")
        .to_owned();
    ctx.hub
        .test_apply_gate_result(&job_id, passed)
        .await
        .unwrap();
    let again = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        again["logObjectId"],
        json!(object_id),
        "the duplicate redelivery must not rebind the Canceled evidence: {again}"
    );
    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    assert_eq!(
        served["log"]["headline"],
        json!("green when the cancel landed"),
        "the Canceled evidence is the green run's own bytes: {served}"
    );
}

/// r22 item 2: a direct supplied-log `canceled` verdict on a Running row is a
/// definitive settlement and must bind its own persisted digest. With a
/// failed insert it defers (Running); redelivery after recovery settles
/// Canceled exactly once.
#[tokio::test]
async fn a_supplied_log_canceled_verdict_requires_its_own_persisted_digest() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/supplied-canceled","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued verify job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    let canceled = |headline: &str| {
        json!({
            "jobId": job_id,
            "generation": 1,
            "status": "canceled",
            "error": "canceled by the operator",
            "runLog": {
                "step": "cargo-test",
                "kind": "failed",
                "attempts": 1,
                "headline": headline,
                "summary": ["run interrupted before completion"],
                "tail": ["canceled"],
                "capturedLines": 1,
                "truncated": false
            }
        })
    };

    // A failed persist defers the Canceled settlement.
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, canceled("the canceled run"))
        .await
        .unwrap();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "a supplied-log canceled verdict must defer when its log does not persist: {doc}"
    );
    assert!(doc["logObjectId"].is_null(), "{doc}");

    // Recovery + redelivery settles Canceled bound to its own object.
    ctx.store().test_clear_gate_log_insert_fault();
    let verdict = canceled("the canceled run");
    ctx.hub
        .test_apply_gate_result(&job_id, verdict.clone())
        .await
        .unwrap();
    let settled = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(settled["state"], json!("canceled"), "{settled}");
    let object_id = settled["logObjectId"]
        .as_str()
        .expect("the Canceled settlement binds its persisted log")
        .to_owned();
    // A further redelivery settles no second time and keeps the object.
    ctx.hub
        .test_apply_gate_result(&job_id, verdict)
        .await
        .unwrap();
    let again = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(again["state"], json!("canceled"), "{again}");
    assert_eq!(
        again["logObjectId"],
        json!(object_id),
        "the duplicate redelivery rebound the Canceled evidence: {again}"
    );
    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    assert_eq!(
        served["log"]["headline"],
        json!("the canceled run"),
        "the Canceled evidence is the verdict's own bytes: {served}"
    );
}

/// The Node delivers a FAILED verdict with runLog through BOTH real Hub
/// entry points — the `gate.event` finished NOTIFICATION and the gate.run
/// RPC REPLY — not a shared in-process helper. The event lands while the
/// job is Running. Deterministic barriers at apply_result's real
/// pre-read/pre-write boundaries hold BOTH arrivals while the row is still
/// Running: the first commits its reference, the second persists its bytes
/// and only then is allowed to reach the terminal guard. The stored
/// logObjectId resolves by object id AND by job id. No SQL writes to job
/// state.
#[tokio::test]
async fn the_finished_event_and_the_gate_run_reply_both_feed_the_real_log_path() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    let job_seed = "wt/gate/two-arrival-paths";
    let hook = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    // Enqueue WITHOUT the barrier first so the scheduler can claim+dispatch.
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook.set_result(failed_gate_result(&job_id, job_seed));
    hook.wait_arrived().await;
    let running = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(running["state"], json!("running"), "{running}");
    // The dispatch is parked at the Node; NOW arm the result barrier before
    // either verdict arrives.
    barrier.arm();

    // ENTRY POINT 1: the real `gate.event` finished notification.
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    // ENTRY POINT 2: release the parked gate.run RPC so its reply — the
    // second real entry — is delivered. The pre-read barrier stops BOTH
    // after they observe the still-Running row (neither commits yet).
    // Bounded release + the barrier's own bounded wait_parked prove the
    // parked frame proceeded (r14 item 8d).
    hook.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    assert_eq!(
        ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["state"],
        json!("running"),
        "both arrivals must observe a Running row"
    );
    // Release BOTH: each persists its log (stable id, identical digest),
    // the first writer commits Failed and the second stops at the terminal
    // guard.
    barrier.release();

    // r11 item 7: wait for BOTH apply_result calls to fully return before
    // reading the lookup routes (no premature assertion on one arrival).
    barrier.wait_completed(2).await;

    // The terminal commit is present and the reference resolvable.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("failed"), "{doc}");
    let object_id = doc["logObjectId"]
        .as_str()
        .expect("the job never settled Failed with a log object")
        .to_owned();
    assert!(object_id.starts_with("obj_"), "{object_id}");

    // The gate.run frame really crossed the link.
    let mut saw_gate_run = false;
    while let Ok((method, _params)) =
        tokio::time::timeout(Duration::from_millis(200), node.next_frame()).await
    {
        if method == "gate.run" {
            saw_gate_run = true;
        }
    }
    assert!(
        saw_gate_run,
        "the Hub never dispatched gate.run over the link"
    );

    // After BOTH arrivals the log resolves by object id and by job id.
    let (status, by_object) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "log by object id: {by_object}");
    assert_eq!(by_object["objectId"], json!(object_id), "{by_object}");
    assert_eq!(
        by_object["log"]["headline"],
        json!("the two-path evidence"),
        "{by_object}"
    );
    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "log by job id: {by_job}");
    assert_eq!(
        by_job["objectId"],
        json!(object_id),
        "by-job lookup resolves elsewhere: {by_job}"
    );
    let final_doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(final_doc["state"], json!("failed"), "{final_doc}");
    assert_eq!(
        final_doc["logObjectId"],
        json!(object_id),
        "the gate.run reply disturbed the reference: {final_doc}"
    );
}

/// r23 item 1 (r22 review defect 1; coordinator clarification of R9): the two
/// REAL deliveries of one Failed-with-log verdict — the `gate.event` finished
/// NOTIFICATION and the gate.run RPC REPLY — BOTH hit transient evidence
/// persistence failures. The Node retains no copy and redelivers nothing, so
/// after both deliveries defer the Hub must not leave a silently-Running row:
/// it records an explicit, PERSISTED uncertain condition (second deferral),
/// keeps the job Running with its lane occupancy, and exposes operator
/// resolution. Storage recovering changes nothing; the operator settles the
/// job Failed, and a later redelivery (which production never sends) stands
/// down idempotently against the terminal row.
#[tokio::test]
async fn failed_verdict_without_persisted_evidence_becomes_persistently_uncertain() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    let job_seed = "wt/gate/evidence-uncertain-failed";
    let hook = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook.set_result(failed_gate_result(&job_id, job_seed));
    hook.wait_arrived().await;

    // Park BOTH real arrivals (notification + RPC reply) after the pre-read,
    // then fail exactly the next two inserts — one per delivery.
    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    hook.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;
    assert_eq!(
        ctx.store().test_gate_log_insert_faults_consumed(),
        2,
        "both original deliveries each consumed one failed insert"
    );

    // The job is PERSISTENTLY UNCERTAIN: still Running, still holding the
    // lane/host occupancy, carrying the explicit condition for the operator.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert!(
        doc["hostId"].is_string(),
        "occupancy (host) retained: {doc}"
    );
    assert_eq!(
        doc["laneId"],
        json!("lane1"),
        "lane occupancy retained: {doc}"
    );
    assert!(doc["logObjectId"].is_null(), "{doc}");
    let condition = &doc["evidenceUncertain"];
    assert_eq!(condition["generation"], json!(1), "{doc}");
    assert_eq!(condition["status"], json!("failed"), "{doc}");
    assert_eq!(condition["deliveriesDeferred"], json!(2), "{doc}");
    let digest = condition["suppliedDigest"]
        .as_str()
        .expect("the condition names the supplied log digest")
        .to_owned();
    assert_eq!(digest.len(), 64, "a sha256 hex digest: {digest}");
    assert!(
        condition["reason"]
            .as_str()
            .unwrap_or("")
            .contains("could not be persisted"),
        "{doc}"
    );
    assert!(condition["firstAt"].is_string(), "{doc}");
    assert!(condition["updatedAt"].is_string(), "{doc}");

    // Storage recovers and the carrier stays connected, but there is no Hub
    // retry and the Node sends nothing again: the condition is durable.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(
        doc["evidenceUncertain"]["deliveriesDeferred"],
        json!(2),
        "{doc}"
    );
    assert!(doc["logObjectId"].is_null(), "{doc}");

    // Only the operator resolution settles it. An AGENT caller is refused;
    // empty rationale and a wrong generation are refused too.
    let resolve_path = format!(
        "/v1/projects/{}/gate/jobs/{job_id}/resolve-evidence",
        ctx.project
    );
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.agent,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "rationale": "agent cannot resolve",
            })),
        )
        .await;
    assert_eq!(
        status, 403,
        "agent callers may not resolve uncertainty: {reply}"
    );
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({"resolution": "failed", "expectedAttempts": 1, "rationale": "  "})),
        )
        .await;
    assert_eq!(status, 400, "{reply}");
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 9,
                "rationale": "wrong generation",
            })),
        )
        .await;
    assert_eq!(status, 409, "a stale generation is refused: {reply}");
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "refused resolutions keep it parked: {doc}"
    );

    // The operator settles Failed with a persisted rationale; the condition
    // clears and the occupancy releases.
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "rationale": "operator verified the cargo failure out of band; no redelivery possible",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("failed"), "{reply}");
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or("")
            .contains("no redelivery possible"),
        "the rationale is persisted on the row: {reply}"
    );

    // A later redelivery (production never sends one; this proves the
    // idempotency the clarification requires) stands down against the
    // terminal row and never manufactures evidence.
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let final_doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(final_doc["state"], json!("failed"), "{final_doc}");
    assert!(
        final_doc["logObjectId"].is_null(),
        "the terminal operator \
        resolution is never rebound by a late frame: {final_doc}"
    );
}

/// r24 item 1: carrier recovery must not discard a persisted evidence-uncertain
/// condition. Both deliveries of a definitive failed verdict defer (inserts
/// fail), the job parks Running with its condition; a cancel then parks it
/// Canceling, and the carrier disconnects while that verdict is still
/// unpersisted. Recovery must NOT finish the row Canceled and release its
/// occupancy: it stays Canceling, still carrying the condition for the
/// operator, exactly as the cancel-grace finisher and the startup reconcile
/// already preserve such rows.
#[tokio::test]
async fn carrier_disconnect_preserves_an_evidence_uncertain_canceling_row() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    let job_seed = "wt/gate/evidence-uncertain-disconnect";
    let hook = node
        .arm_gate_run(failed_gate_result("placeholder", job_seed))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook.set_result(failed_gate_result(&job_id, job_seed));
    hook.wait_arrived().await;

    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    hook.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["evidenceUncertain"]["generation"], json!(1), "{doc}");
    assert_eq!(
        doc["evidenceUncertain"]["deliveriesDeferred"],
        json!(2),
        "{doc}"
    );

    // A cancel arrives while the definitive verdict is parked.
    let (status, reply) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate/jobs/{job_id}/cancel", ctx.project),
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("canceling"), "{reply}");
    assert!(reply["evidenceUncertain"].is_object(), "{reply}");

    // The carrier drops. Recovery must leave the row parked Canceling with its
    // occupancy and condition, not finish it Canceled.
    node.disconnect();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceling"),
        "carrier recovery must not finish an evidence-uncertain Canceling row: {doc}"
    );
    assert_eq!(doc["hostId"], json!(ctx.host), "occupancy retained: {doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "lane retained: {doc}");
    assert!(
        doc["evidenceUncertain"].is_object(),
        "the condition survives carrier recovery: {doc}"
    );
    assert_eq!(doc["evidenceUncertain"]["generation"], json!(1), "{doc}");

    // A second wait through scheduler ticks changes nothing.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("canceling"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");
}

/// r26 item 2 (a): an evidence-uncertain CANCELING verify job keeps its LANE
/// checkout (`busy_lanes`). The candidate is a verify pinned to the SAME lane
/// and is the ONLY queued job — no earlier blocked job can break the project
/// FIFO loop and mask the check. It must not dispatch while the condition
/// stands (even though a verify candidate holds no land slot at all) and runs
/// on the lane only after an authorized settlement releases A.
#[tokio::test]
async fn an_evidence_uncertain_canceling_verify_job_keeps_its_lane_occupancy() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    // Job A: a verify pinned to lane1 whose definitive verdict cannot persist.
    let seed_a = "wt/gate/evidence-lane-occupancy-a";
    let hook_a = node
        .arm_gate_run(failed_gate_result("placeholder", seed_a))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":seed_a,"mode":"verify","laneId":"lane1"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_a = enqueued["id"].as_str().unwrap().to_owned();
    hook_a.set_result(failed_gate_result(&job_a, seed_a));
    hook_a.wait_arrived().await;
    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_a,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_a, seed_a),
        }
    }));
    hook_a.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;
    let doc = ctx.hub.test_get_gate_job(&job_a).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["mode"], json!("verify"), "{doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // A cancel moves it to Canceling WITH the parked condition.
    let (status, reply) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate/jobs/{job_a}/cancel", ctx.project),
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("canceling"), "{reply}");
    assert!(reply["evidenceUncertain"].is_object(), "{reply}");

    // The candidate: a verify for the SAME lane, and the ONLY queued job, so
    // FIFO blocking cannot mask the busy-lane check.
    let hook_b = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    let seed_b = "wt/gate/evidence-lane-occupancy-b";
    let (status, enqueued_b) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":seed_b,"mode":"verify","laneId":"lane1"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued_b}");
    let job_b = enqueued_b["id"].as_str().unwrap().to_owned();

    // Across scheduler ticks the candidate stays queued and no gate.run moves.
    assert!(
        tokio::time::timeout(Duration::from_millis(1400), hook_b.wait_arrived())
            .await
            .is_err(),
        "the verify for the parked job's lane must not dispatch while A is uncertain Canceling"
    );
    let doc_b = ctx.hub.test_get_gate_job(&job_b).await.unwrap().unwrap();
    assert_eq!(
        doc_b["state"],
        json!("queued"),
        "B blocked by the busy lane: {doc_b}"
    );

    // An authorized exact-tuple settlement releases A (Failed).
    let (status, reply) = ctx
        .send(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{job_a}/resolve-evidence",
                ctx.project
            ),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "rationale": "operator settled the parked canceling verify",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("failed"), "{reply}");
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");

    // The candidate now dispatches once, taking the released lane.
    hook_b.wait_arrived().await;
    let doc_b = async {
        loop {
            let doc = ctx.hub.test_get_gate_job(&job_b).await.unwrap().unwrap();
            if doc["state"] == json!("running") {
                break doc;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    .await;
    assert_eq!(doc_b["attempts"], json!(1), "B dispatched once: {doc_b}");
    assert_eq!(
        doc_b["laneId"],
        json!("lane1"),
        "B takes A's old lane: {doc_b}"
    );
}

/// r26 item 2 (b): an evidence-uncertain CANCELING LAND job keeps the
/// project's single LAND SLOT (`land_active`). Two lanes are configured; the
/// candidate is a land pinned to the OTHER, FREE lane and is the ONLY queued
/// job. Its lane is available, so only the land-slot occupancy can block it:
/// it must not dispatch while the condition stands and runs only after an
/// authorized settlement releases A.
#[tokio::test]
async fn an_evidence_uncertain_canceling_land_job_keeps_the_project_land_slot() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    // Two lanes on the one connected host.
    let (status, body) = ctx
        .send(
            "PATCH",
            &format!("/v1/projects/{}", ctx.project),
            &ctx.human,
            None,
            Some(json!({
                "gate": {
                    "affected": true,
                    "web": "auto",
                    "landSerialization": "global-cas",
                    "lanes": [
                        {
                            "id": "lane1",
                            "hostId": ctx.host,
                            "repoPath": "/tmp/lane1/repo",
                            "targetDir": "/tmp/lane1/target",
                            "ports": "58400-58409",
                            "env": {},
                            "lockPath": "/tmp/remuda-agents/e2e.lock",
                            "pwEndpoint": "ws://127.0.0.1:3177/"
                        },
                        {
                            "id": "lane2",
                            "hostId": ctx.host,
                            "repoPath": "/tmp/lane2/repo",
                            "targetDir": "/tmp/lane2/target",
                            "ports": "58410-58419",
                            "env": {},
                            "lockPath": "/tmp/remuda-agents/e2e-2.lock",
                            "pwEndpoint": "ws://127.0.0.1:3177/"
                        }
                    ]
                }
            })),
        )
        .await;
    assert_eq!(status, 200, "configure two lanes: {body}");
    let barrier = ctx.store().gate_apply_barrier();

    // Job A: a land pinned to lane1 whose definitive verdict cannot persist.
    let seed_a = "wt/gate/evidence-landslot-occupancy-a";
    let hook_a = node
        .arm_gate_run(failed_gate_result("placeholder", seed_a))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":seed_a,"mode":"land","laneId":"lane1"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_a = enqueued["id"].as_str().unwrap().to_owned();
    hook_a.set_result(failed_gate_result(&job_a, seed_a));
    hook_a.wait_arrived().await;
    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_a,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_a, seed_a),
        }
    }));
    hook_a.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;
    let doc = ctx.hub.test_get_gate_job(&job_a).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["mode"], json!("land"), "{doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // A cancel moves it to Canceling WITH the parked condition.
    let (status, reply) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate/jobs/{job_a}/cancel", ctx.project),
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("canceling"), "{reply}");
    assert!(reply["evidenceUncertain"].is_object(), "{reply}");

    // The candidate: a land for the FREE lane2, and the ONLY queued job. Its
    // lane is unoccupied, so the land slot is the only thing that can block.
    let hook_b = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    let seed_b = "wt/gate/evidence-landslot-occupancy-b";
    let (status, enqueued_b) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":seed_b,"mode":"land","laneId":"lane2"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued_b}");
    let job_b = enqueued_b["id"].as_str().unwrap().to_owned();

    // Across scheduler ticks the land candidate stays queued and no gate.run.
    assert!(
        tokio::time::timeout(Duration::from_millis(1400), hook_b.wait_arrived())
            .await
            .is_err(),
        "the land for the FREE lane must not dispatch while A holds the project land slot"
    );
    let doc_b = ctx.hub.test_get_gate_job(&job_b).await.unwrap().unwrap();
    assert_eq!(
        doc_b["state"],
        json!("queued"),
        "B blocked by the project land slot: {doc_b}"
    );

    // An authorized exact-tuple settlement releases A (Failed).
    let (status, reply) = ctx
        .send(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{job_a}/resolve-evidence",
                ctx.project
            ),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "rationale": "operator settled the parked canceling land",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("failed"), "{reply}");
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");

    // The land candidate now dispatches once on the free lane.
    hook_b.wait_arrived().await;
    let doc_b = async {
        loop {
            let doc = ctx.hub.test_get_gate_job(&job_b).await.unwrap().unwrap();
            if doc["state"] == json!("running") {
                break doc;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    .await;
    assert_eq!(doc_b["attempts"], json!(1), "B dispatched once: {doc_b}");
    assert_eq!(
        doc_b["laneId"],
        json!("lane2"),
        "B takes the free lane: {doc_b}"
    );
}

/// r25 item 1: an evidence-uncertain RUNNING row whose carrier is lost is
/// PARKED — it stays Running, keeps its lane/host occupancy and its binding to
/// attempt 1, and is never re-dispatched. A carrier loss is neither an
/// authorized settlement nor an exact-tuple operator resolution: when the
/// carrier returns the scheduler must NOT claim a fresh attempt (no second
/// gate.run ever goes out) and `attempts` never advances while the condition
/// stands. Only an authorized operator resolution for the exact tuple
/// releases it.
#[tokio::test]
async fn carrier_loss_parks_an_evidence_uncertain_running_row_and_reconnect_dispatches_nothing() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    let job_seed = "wt/gate/evidence-uncertain-parked-run";
    let hook = node
        .arm_gate_run(failed_gate_result("placeholder", job_seed))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook.set_result(failed_gate_result(&job_id, job_seed));
    hook.wait_arrived().await;

    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    hook.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["evidenceUncertain"]["generation"], json!(1), "{doc}");

    // The carrier drops. Recovery must leave the row PARKED Running with its
    // occupancy and condition — not requeue it and release the lane.
    node.disconnect();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "carrier recovery must not requeue an evidence-uncertain Running row: {doc}"
    );
    assert_eq!(
        doc["attempts"],
        json!(1),
        "the attempt binding is kept: {doc}"
    );
    assert_eq!(
        doc["hostId"],
        json!(ctx.host),
        "host occupancy retained: {doc}"
    );
    assert_eq!(
        doc["laneId"],
        json!("lane1"),
        "lane occupancy retained: {doc}"
    );
    assert!(
        doc["evidenceUncertain"].is_object(),
        "the condition survives carrier recovery: {doc}"
    );
    assert_eq!(doc["evidenceUncertain"]["generation"], json!(1), "{doc}");

    // The carrier returns. Arm a gate.run hook BEFORE the hello so any
    // unauthorized re-dispatch latches: none must arrive across scheduler
    // ticks, and the attempt must never advance.
    let node_b = FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace)
        .await
        .unwrap();
    let hook2 = node_b
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1400), hook2.wait_arrived())
            .await
            .is_err(),
        "reconnect must not dispatch a fresh attempt for a parked uncertain job"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "still parked after reconnect: {doc}"
    );
    assert_eq!(
        doc["attempts"],
        json!(1),
        "no new attempt was claimed: {doc}"
    );
    assert_eq!(doc["hostId"], json!(ctx.host), "{doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "{doc}");
    assert_eq!(doc["evidenceUncertain"]["generation"], json!(1), "{doc}");

    // Only an exact-tuple operator resolution releases the parked row.
    let (status, reply) = ctx
        .send(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{job_id}/resolve-evidence",
                ctx.project
            ),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "rationale": "operator settled the parked attempt out of band",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("failed"), "{reply}");
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");
    // The terminal row still never produced a second dispatch.
    assert!(
        tokio::time::timeout(Duration::from_millis(600), hook2.wait_arrived())
            .await
            .is_err(),
        "settling the parked attempt must not dispatch anything either"
    );
}

/// r24 item 1: after the finished notification has parked an evidence-uncertain
/// condition, loss of the outstanding gate.run RPC reply must not let the
/// RPC-error writer settle the job Failed without its evidence.
#[tokio::test]
async fn a_lost_rpc_reply_while_evidence_uncertain_preserves_the_condition() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;

    let job_seed = "wt/gate/evidence-uncertain-rpc-lost";
    let hook = node
        .arm_gate_run(failed_gate_result("placeholder", job_seed))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook.set_result(failed_gate_result(&job_id, job_seed));
    hook.wait_arrived().await;

    // The finished notification is the FIRST delivery and its single evidence
    // insert fails (one-shot fault armed while the RPC reply is still parked),
    // so the definitive verdict defers with deliveriesDeferred=1.
    ctx.store().test_arm_gate_log_insert_faults(1);
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
        if doc["evidenceUncertain"]["deliveriesDeferred"] == json!(1) {
            assert_eq!(doc["state"], json!("running"), "{doc}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never deferred: {doc}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(ctx.store().test_gate_log_insert_faults_consumed(), 1);

    // The outstanding gate.run reply is lost: the fake answers with a real
    // JSON-RPC error frame, driving fail_rpc through its production callback.
    hook.reply_error();
    hook.release_and_wait_proceeded().await;
    let rpc_barrier = ctx.store().gate_apply_barrier();
    let before = rpc_barrier.rpc_completed_count();
    rpc_barrier.wait_rpc_completed(before + 1).await;

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "the RPC-error writer must not fail an evidence-uncertain job: {doc}"
    );
    assert!(
        doc["evidenceUncertain"].is_object(),
        "the parked condition survives the lost reply: {doc}"
    );
    assert_eq!(doc["evidenceUncertain"]["generation"], json!(1), "{doc}");
}

/// r23 item 1: the same persistent condition for a Passed-with-keepLogs
/// verdict (a green run whose `kept` log cannot be persisted). If a
/// redelivery EVER arrives later it remains idempotent: storage recovered,
/// the single redelivery settles Passed exactly once bound to its OWN newly
/// persisted object and clears the uncertain condition; a duplicate changes
/// nothing. No Hub-side retry manufactures that delivery.
#[tokio::test]
async fn passed_keep_logs_without_persisted_evidence_is_uncertain_then_redelivery_settles_once() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    let job_seed = "wt/gate/evidence-uncertain-passed";
    let passed_result = json!({
        "jobId": "placeholder",
        "generation": 1,
        "status": "passed",
        "runLog": {
            "step": "*",
            "kind": "kept",
            "attempts": 1,
            "headline": "green; log retained by --keep-logs",
            "summary": ["cargo test ... ok"],
            "tail": ["Finished gate successfully"],
            "capturedLines": 1,
            "truncated": false
        }
    });
    let hook = node.arm_gate_run(passed_result.clone()).await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    let mut for_job = passed_result.clone();
    for_job["jobId"] = json!(job_id);
    hook.set_result(for_job.clone());
    hook.wait_arrived().await;

    barrier.arm();
    let mut event = json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": {},
        }
    });
    event["params"]["result"] = for_job.clone();
    node.send_raw(event);
    hook.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;
    assert_eq!(ctx.store().test_gate_log_insert_faults_consumed(), 2);

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "occupancy retained: {doc}");
    assert_eq!(doc["evidenceUncertain"]["status"], json!("passed"), "{doc}");
    assert_eq!(
        doc["evidenceUncertain"]["deliveriesDeferred"],
        json!(2),
        "{doc}"
    );

    // Recovery alone does not heal it.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // One later redelivery settles Passed exactly once and clears the
    // condition; the dual delivery this replaces is NOT manufactured by the
    // Hub — nothing self-settled in the wait above.
    let mut redelivery = json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": {},
        }
    });
    redelivery["params"]["result"] = for_job.clone();
    node.send_raw(redelivery);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let settled = loop {
        let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
        if doc["state"] == json!("passed") {
            break doc;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never settled: {doc}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        settled["evidenceUncertain"].is_null(),
        "settlement clears the condition: {settled}"
    );
    let object_id = settled["logObjectId"]
        .as_str()
        .expect("bound to its own object");
    assert!(object_id.starts_with("obj_"), "{object_id}");

    // A duplicate redelivery is a no-op: same terminal state, same object.
    let mut duplicate = json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": {},
        }
    });
    duplicate["params"]["result"] = for_job;
    node.send_raw(duplicate);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let final_doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(final_doc["state"], json!("passed"), "{final_doc}");
    assert_eq!(final_doc["logObjectId"], json!(object_id), "{final_doc}");
    assert!(final_doc["evidenceUncertain"].is_null(), "{final_doc}");
}

/// r23 item 1: the operator `requeue` resolution releases the parked
/// occupancy and the fresh attempt runs; the uncertain condition and the
/// stale evidence reference are cleared.
#[tokio::test]
async fn operator_requeue_releases_a_persistently_uncertain_jobs_occupancy() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/evidence-uncertain-requeue","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let job_id = body["id"].as_str().unwrap().to_owned();
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("the queued verify job claims to running");
    assert_eq!(claimed["state"], json!("running"), "{claimed}");

    // Both real handler entries (the notification and the reply) fail their
    // insert against the sticky fault.
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, failed_gate_result(&job_id, "x"))
        .await
        .unwrap();
    ctx.hub
        .test_apply_gate_result(&job_id, failed_gate_result(&job_id, "x"))
        .await
        .unwrap();
    ctx.store().test_clear_gate_log_insert_fault();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(
        doc["evidenceUncertain"]["deliveriesDeferred"],
        json!(2),
        "{doc}"
    );

    let (status, reply) = ctx
        .send(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{job_id}/resolve-evidence",
                ctx.project
            ),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "requeue",
                "expectedAttempts": 1,
                "rationale": "run it again; storage is healthy now",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("queued"), "{reply}");
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");
    assert!(
        reply["hostId"].is_null(),
        "the lane occupancy is released: {reply}"
    );
    assert!(reply["laneId"].is_null(), "{reply}");
    assert!(
        reply["logObjectId"].is_null(),
        "stale evidence reference cleared: {reply}"
    );
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or("")
            .contains("run it again"),
        "the rationale is persisted: {reply}"
    );
}

/// r25 item 1 / r24 item 2: an evidence-uncertain condition is bound to the
/// CURRENT attempt's full (attempt, epoch, merge) tuple, and a parked
/// uncertain job is never re-dispatched across a carrier loss. Attempt N parks
/// the condition; disconnect + reconnect must not claim attempt N+1. Only an
/// AUTHORIZED operator requeue for the exact N tuple clears the condition
/// BEFORE attempts advance, after which the scheduler claims a clean attempt
/// N+1 and grants it a FRESH Landing (epoch/merge). Any delayed resolution
/// still naming attempt N is then refused with 409 and must NOT abort or settle
/// the newer attempt.
#[tokio::test]
async fn a_stale_evidence_resolution_for_attempt_n_does_not_touch_attempt_n_plus_one() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let barrier = ctx.store().gate_apply_barrier();

    let job_seed = "wt/gate/evidence-stale-tuple";
    let hook = node
        .arm_gate_run(failed_gate_result("placeholder", job_seed))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":job_seed,"mode":"land"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook.set_result(failed_gate_result(&job_id, job_seed));
    hook.wait_arrived().await;

    // Both deliveries of attempt N's definitive verdict defer: attempt N
    // parks evidence-uncertain (tuple (1, null, null)).
    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 1,
            "kind": "finished",
            "result": failed_gate_result(&job_id, job_seed),
        }
    }));
    hook.release_and_wait_proceeded().await;
    barrier.wait_parked(2).await;
    ctx.store().test_arm_gate_log_insert_faults(2);
    barrier.release();
    barrier.wait_completed(2).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["attempts"], json!(1), "{doc}");

    // A stale resolution (naming attempt 2 while attempt 1 is parked) is a
    // tuple 409 and changes nothing.
    let resolve_path = format!(
        "/v1/projects/{}/gate/jobs/{job_id}/resolve-evidence",
        ctx.project
    );
    let (status, _reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 2,
                "rationale": "jumping ahead of the parked attempt",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "a resolution naming a non-current attempt is refused while parked"
    );

    // Carrier loss + return: the row stays PARKED Running — no recovery
    // requeue, no reclaim, no second gate.run, no attempt advance.
    node.disconnect();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "parked through recovery: {doc}"
    );
    assert_eq!(doc["attempts"], json!(1), "{doc}");
    let mut node_b = FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace)
        .await
        .unwrap();
    let hook2 = node_b
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), hook2.wait_arrived())
            .await
            .is_err(),
        "reconnect alone must not dispatch attempt N+1 while the condition stands"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["attempts"], json!(1), "still attempt N: {doc}");
    assert_eq!(
        doc["evidenceUncertain"]["generation"],
        json!(1),
        "the condition for attempt N is still attached: {doc}"
    );

    // Only the AUTHORIZED requeue naming the exact N tuple releases it; the
    // condition clears BEFORE the attempt can advance.
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "requeue",
                "expectedAttempts": 1,
                "rationale": "storage healed; run the attempt again cleanly",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("queued"), "{reply}");
    assert!(
        reply["evidenceUncertain"].is_null(),
        "condition cleared first: {reply}"
    );
    assert_eq!(
        reply["attempts"],
        json!(1),
        "attempts advance only at claim: {reply}"
    );

    // The scheduler now really claims and dispatches a CLEAN attempt N+1.
    hook2.wait_arrived().await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("running"), "{doc}");
    assert_eq!(doc["attempts"], json!(2), "attempt N+1 claimed: {doc}");
    assert!(
        doc["evidenceUncertain"].is_null(),
        "the fresh attempt carries no stale condition: {doc}"
    );

    // Attempt N+1 is granted Landing with a fresh epoch and merge.
    let session_b = ctx.hub.test_node_session(&ctx.host).await.expect("B live");
    let merge2 = "dddddddddddddddddddddddddddddddddddddddd";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge2, session_b)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    let granted_doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    let epoch2 = granted_doc["landingSessionEpoch"]
        .as_str()
        .expect("fresh grant epoch")
        .to_owned();
    let doc = granted_doc;
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert_eq!(doc["attempts"], json!(2), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge2), "{doc}");

    // The delayed resolution still names attempt N (1) without an epoch/merge
    // (N never reached Landing): refused with no abort — a Landing resolution
    // must name the full tuple, so it cannot even reach the abort.
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "rationale": "delayed resolution for the lost attempt",
            })),
        )
        .await;
    assert_eq!(
        status, 400,
        "a stale attempt-N Landing resolution lacking the tuple is refused: {reply}"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "N+1 Landing untouched: {doc}"
    );
    assert_eq!(doc["attempts"], json!(2), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge2), "{doc}");

    // No abort frame for N+1 was sent (drain whatever the reconnect produced).
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    while tokio::time::Instant::now() < drain_deadline {
        if let Ok((method, _)) =
            tokio::time::timeout(Duration::from_millis(200), node_b.next_frame()).await
        {
            assert_ne!(
                method, "gate.abortLanding",
                "the stale resolution must not abort the attempt-N+1 land run"
            );
        }
    }

    // A Landing resolution missing the tuple is a 400; a tuple naming N with
    // the epoch/merge N never had is a stale-tuple 409.
    let (status, _reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 2,
                "rationale": "landing without tuple",
            })),
        )
        .await;
    assert_eq!(status, 400, "a Landing resolution must pin epoch and merge");
    let (status, _reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch2,
                "expectedMergeSha": merge2,
                "rationale": "mismatched tuple",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "naming N with the N+1 epoch/merge still mismatches the live row"
    );

    // The FULL live tuple is refused too: the authorized requeue cleared the
    // condition, so only N+1's OWN settlement can move the Landing. No abort
    // was ever sent.
    let (status, _reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 2,
                "expectedSessionEpoch": epoch2,
                "expectedMergeSha": merge2,
                "rationale": "current attempt without a parked condition",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "without a parked condition only the attempt's own settlement acts"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert_eq!(doc["attempts"], json!(2), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge2), "{doc}");
    assert!(doc["evidenceUncertain"].is_null(), "{doc}");

    // N+1's OWN definitive (non-landed) settlement settles the current attempt
    // — no abort frame either, since this is the live verdict writer, not the
    // operator route.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "generation": 2,
                "status": "base-moved",
                "mergeSha": merge2,
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567",
            }),
        )
        .await
        .unwrap();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("queued"),
        "N+1 base-moved requeues: {doc}"
    );
    assert_eq!(doc["attempts"], json!(2), "{doc}");
    assert!(doc["evidenceUncertain"].is_null(), "{doc}");
}

/// r24 item 3: a keepLogs land PUSHED the exact granted merge to remote main,
/// but BOTH supplied-log inserts failed, so the row stays Landing with
/// evidence uncertainty. When the operator then requests `failed` (or
/// `requeue`), the resolver must run its read-only remote-main probe FIRST and
/// settle Landed — an established landed fact beats the operator override; the
/// merge is never marked failed or re-pushed.
#[tokio::test]
async fn an_evidence_resolution_settles_landed_when_the_merge_is_already_on_remote_main() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx
        .hub
        .test_node_session(&ctx.host)
        .await
        .expect("carrier live");
    let merge = "cccccccccccccccccccccccccccccccccccccccc";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    let epoch = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["landingSessionEpoch"]
        .as_str()
        .expect("grant epoch")
        .to_owned();

    // Both real verdict deliveries (finished notification + gate.run reply)
    // land with the pushed merge and a keepLogs (`kept`) log whose inserts
    // both fail.
    let landed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "landed",
        "mergeSha": merge,
        "runLog": {
            "step": "*", "kind": "kept", "headline": "landed; log retained by --keep-logs",
            "summary": [], "tail": ["Finished land successfully"],
            "capturedLines": 1, "truncated": false, "attempts": 1,
        },
    });
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, landed_result.clone())
        .await
        .unwrap();
    ctx.hub
        .test_apply_gate_result(&job_id, landed_result.clone())
        .await
        .unwrap();
    ctx.store().test_clear_gate_log_insert_fault();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge), "{doc}");
    assert_eq!(doc["evidenceUncertain"]["status"], json!("landed"), "{doc}");
    assert_eq!(
        doc["evidenceUncertain"]["deliveriesDeferred"],
        json!(2),
        "{doc}"
    );

    // The read-only probe now establishes the exact granted merge on remote
    // main.
    node.set_resolve_reply(json!({
        "jobId": job_id, "status": "landed",
        "baseSha": "0123456789abcdef0123456789abcdef01234567",
    }))
    .await;

    // The operator believes the evidence loss means the run failed and asks
    // for `failed`; the landed fact wins.
    let resolve_path = format!(
        "/v1/projects/{}/gate/jobs/{job_id}/resolve-evidence",
        ctx.project
    );
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator thought the missing keepLogs evidence meant failure",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("landed"), "landed fact wins: {reply}");
    assert_eq!(reply["mergeSha"], json!(merge), "{reply}");
    assert!(
        reply["evidenceUncertain"].is_null(),
        "condition cleared: {reply}"
    );
    assert!(
        reply["error"].as_str().unwrap_or("").is_empty()
            || !reply["error"].as_str().unwrap_or("").contains("operator"),
        "no operator-failed error is recorded when the probe established landed: {reply}"
    );

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landed"), "{doc}");
    assert!(doc["evidenceUncertain"].is_null(), "{doc}");

    // The same fact also beats `requeue`: a fresh row parked the same way
    // stays Landed rather than re-running the pushed merge.
    let job2 = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job2, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session2 = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let grant2 = ctx
        .hub
        .test_gate_land_grant(&job2, &ctx.host, merge, session2)
        .await
        .unwrap();
    assert_eq!(grant2["granted"], json!(true), "{grant2}");
    let epoch2 = ctx.hub.test_get_gate_job(&job2).await.unwrap().unwrap()["landingSessionEpoch"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut landed2 = landed_result.clone();
    landed2["jobId"] = json!(&job2);
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job2, landed2.clone())
        .await
        .unwrap();
    ctx.hub
        .test_apply_gate_result(&job2, landed2.clone())
        .await
        .unwrap();
    ctx.store().test_clear_gate_log_insert_fault();
    node.set_resolve_reply(json!({
        "jobId": job2, "status": "landed",
        "baseSha": "0123456789abcdef0123456789abcdef01234567",
    }))
    .await;
    let (status, reply) = ctx
        .send(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{job2}/resolve-evidence",
                ctx.project
            ),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "requeue",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch2,
                "expectedMergeSha": merge,
                "rationale": "operator wanted a rerun",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        reply["state"],
        json!("landed"),
        "landed beats requeue: {reply}"
    );
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");
}

/// r25 item 2: an UNDECIDABLE remote-main probe must not permit an operator
/// Failed/Requeue override of an evidence-uncertain Landing. A fetch error, a
/// carrier offline, or an Unknown response each leave the Landing, its
/// condition and its lane/land occupancy untouched and return a 409 conflict.
/// Only an AUTHORITATIVE Absent probe (fresh fetch, merge absent, land group
/// verified dead) releases the override; an established Landed fact still
/// wins (covered separately).
#[tokio::test]
async fn an_undecidable_probe_keeps_the_uncertain_landing_and_blocks_failed_or_requeue() {
    async fn park_uncertain_land(ctx: &Ctx, seed: &str) -> (String, String, String) {
        let job_id = queued_land_job(ctx).await;
        ctx.hub
            .test_claim_gate_job(&job_id, "lane1", &ctx.host)
            .await
            .unwrap()
            .expect("claimed");
        let session = ctx
            .hub
            .test_node_session(&ctx.host)
            .await
            .expect("carrier live");
        let merge = format!("{:e>40}", seed);
        let grant = ctx
            .hub
            .test_gate_land_grant(&job_id, &ctx.host, &merge, session)
            .await
            .unwrap();
        assert_eq!(grant["granted"], json!(true), "{grant}");
        let epoch =
            ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["landingSessionEpoch"]
                .as_str()
                .expect("grant epoch")
                .to_owned();
        let landed_result = json!({
            "jobId": job_id,
            "generation": 1,
            "status": "landed",
            "mergeSha": merge,
            "runLog": {
                "step": "*", "kind": "kept", "headline": "landed; log retained by --keep-logs",
                "summary": [], "tail": ["Finished land successfully"],
                "capturedLines": 1, "truncated": false, "attempts": 1,
            },
        });
        ctx.store().test_arm_gate_log_insert_fault_sticky();
        ctx.hub
            .test_apply_gate_result(&job_id, landed_result.clone())
            .await
            .unwrap();
        ctx.hub
            .test_apply_gate_result(&job_id, landed_result)
            .await
            .unwrap();
        ctx.store().test_clear_gate_log_insert_fault();
        let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
        assert_eq!(doc["state"], json!("landing"), "{doc}");
        assert!(doc["evidenceUncertain"].is_object(), "{doc}");
        (job_id, epoch, merge)
    }

    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let (job_id, epoch, merge) = park_uncertain_land(&ctx, "a").await;
    let resolve_path = format!(
        "/v1/projects/{}/gate/jobs/{job_id}/resolve-evidence",
        ctx.project
    );
    let assert_still_parked = |reply: &Value| {
        assert!(
            !reply["state"]
                .as_str()
                .is_some_and(|s| s == "failed" || s == "queued"),
            "an undecidable probe must not settle the Landing: {reply}"
        );
    };

    // 1) Fetch error: the Node reports the recovery fetch failed (Unknown).
    node.set_resolve_reply(json!({
        "jobId": job_id,
        "status": "unknown",
        "error": "fetch origin failed",
    }))
    .await;
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator guessed failure while the fetch was failing",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "a fetch error is undecidable: the override must conflict: {reply}"
    );
    assert_still_parked(&reply);
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "Landing kept after fetch error: {doc}"
    );
    assert_eq!(doc["hostId"], json!(ctx.host), "occupancy kept: {doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "lane kept: {doc}");
    assert!(
        doc["evidenceUncertain"].is_object(),
        "condition kept: {doc}"
    );

    // 2) Plain Unknown response (shallow history / group death unattested /
    // malformed probe) — requeue is blocked just the same.
    node.set_resolve_reply(json!({ "jobId": job_id, "status": "unknown" }))
        .await;
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "requeue",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator wanted a rerun without an authoritative answer",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "an Unknown probe response must not permit a requeue: {reply}"
    );
    assert_still_parked(&reply);
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // 3) Carrier offline: no probe authority at all.
    node.disconnect();
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator tried while the lane host was gone",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "missing probe authority must conflict, not settle: {reply}"
    );
    assert_still_parked(&reply);
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "Landing kept while offline: {doc}"
    );
    assert_eq!(doc["hostId"], json!(ctx.host), "{doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // The carrier returns. The post-hello forced reconcile runs first with no
    // scripted answer (the fake's default Unknown): let it finish and confirm
    // it leaves the row parked while arming the per-row probe cooldown.
    let node_b = FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "Unknown hello pass keeps it: {doc}"
    );
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // The operator probe now AUTHORITATIVELY establishes absence (fresh fetch,
    // merge not on the remote, land group verified dead): the requeue is
    // permitted and clears the condition.
    node_b
        .set_resolve_reply(json!({
            "jobId": job_id,
            "status": "absent",
            "baseSha": null,
            "error": "merge absent and land group verified gone",
        }))
        .await;
    let (status, reply) = ctx
        .send(
            "POST",
            &resolve_path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "requeue",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "absence established; rerun it",
            })),
        )
        .await;
    assert_eq!(
        status, 200,
        "authoritative absence permits the requeue: {reply}"
    );
    assert_eq!(reply["state"], json!("queued"), "{reply}");
    assert!(
        reply["evidenceUncertain"].is_null(),
        "condition cleared: {reply}"
    );
    assert!(reply["hostId"].is_null(), "occupancy released: {reply}");
    assert!(reply["laneId"].is_null(), "{reply}");

    // Authoritative Absent permits Failed too, on a second parked Landing. Let
    // one forced pass consume the fake's default Unknown (arming the probe
    // cooldown so only the endpoint reads the scripted Absent below).
    let (job2, epoch2, merge2) = park_uncertain_land(&ctx, "b").await;
    ctx.hub.test_force_landing_reconcile(&ctx.host).await;
    let doc = ctx.hub.test_get_gate_job(&job2).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "Unknown keeps the second row: {doc}"
    );
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");
    node_b
        .set_resolve_reply(json!({
            "jobId": job2,
            "status": "absent",
            "baseSha": null,
        }))
        .await;
    let (status, reply) = ctx
        .send(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{job2}/resolve-evidence",
                ctx.project
            ),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch2,
                "expectedMergeSha": merge2,
                "rationale": "absence established; settle it failed",
            })),
        )
        .await;
    assert_eq!(
        status, 200,
        "authoritative absence permits the failed override: {reply}"
    );
    assert_eq!(reply["state"], json!("failed"), "{reply}");
    assert!(reply["evidenceUncertain"].is_null(), "{reply}");
}

/// r26 item 1: recovery expiry can leave an evidence-uncertain Landing ALSO
/// flagged `landingOutcomeUnknown` — reconciliation flips the expiry flag
/// without touching the parked condition. The legacy `POST .../resolve` route
/// tolerates an Unknown probe for an ordinary outcome-unknown Landing, which
/// used to let it clear `evidenceUncertain` and release parked lane/land
/// occupancy with no authoritative absence. Such a row is now refused (409)
/// and directed to `/resolve-evidence`: with the carrier answering Unknown and
/// again with the carrier offline, `/resolve` returns 409, sends no abort and
/// leaves the entire row (and its lane occupancy) untouched.
#[tokio::test]
async fn the_legacy_resolve_route_refuses_an_evidence_uncertain_landing_after_recovery_expiry() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;

    // Park a LAND as evidence-uncertain: a landed/keep-logs verdict whose two
    // log-persist attempts both fail leaves the row Landing with a condition.
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx
        .hub
        .test_node_session(&ctx.host)
        .await
        .expect("carrier live");
    let merge = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    let epoch = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["landingSessionEpoch"]
        .as_str()
        .expect("grant epoch")
        .to_owned();
    let landed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "landed",
        "mergeSha": merge,
        "runLog": {
            "step": "*", "kind": "kept", "headline": "landed; log retained by --keep-logs",
            "summary": [], "tail": ["Finished land successfully"],
            "capturedLines": 1, "truncated": false, "attempts": 1,
        },
    });
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, landed_result.clone())
        .await
        .unwrap();
    ctx.hub
        .test_apply_gate_result(&job_id, landed_result)
        .await
        .unwrap();
    ctx.store().test_clear_gate_log_insert_fault();

    // Age the Landing past the recovery window; with the carrier answering
    // Unknown the reconciler ALSO marks it outcome-unknown. Both flags stand.
    ctx.hub
        .test_set_landing_at(&job_id, "2000-01-01T00:00:00Z")
        .await
        .unwrap();
    node.set_resolve_reply(json!({ "jobId": job_id, "status": "unknown" }))
        .await;
    ctx.hub.test_force_landing_reconcile(&ctx.host).await;
    let parked = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(parked["state"], json!("landing"), "{parked}");
    assert_eq!(
        parked["landingOutcomeUnknown"],
        json!(true),
        "recovery expiry set the unknown flag: {parked}"
    );
    assert!(
        parked["evidenceUncertain"].is_object(),
        "the evidence condition survived recovery expiry: {parked}"
    );

    let resolve_path = format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project);
    let request = || {
        json!({
            "resolution": "failed",
            "expectedAttempts": 1,
            "expectedSessionEpoch": epoch,
            "expectedMergeSha": merge,
            "rationale": "operator tried the legacy route while the probe was undecidable",
        })
    };

    // Occupancy is live while the carrier is connected: a verify pinned to
    // lane1 must not dispatch while the row is parked.
    let hook = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/legacy-resolve-blocked","mode":"verify","laneId":"lane1"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let blocker = enqueued["id"].as_str().unwrap().to_owned();

    // 1) Carrier live, probe authority Unknown -> 409 before probe/abort.
    let (status, reply) = ctx
        .send("POST", &resolve_path, &ctx.human, None, Some(request()))
        .await;
    assert_eq!(
        status, 409,
        "an evidence-uncertain Landing is refused on the legacy route: {reply}"
    );
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or("")
            .contains("resolve-evidence"),
        "the conflict directs the operator to the evidence route: {reply}"
    );
    // No abort of the granted land is queued by the refused call.
    let drain_deadline = std::time::Instant::now() + Duration::from_millis(400);
    while std::time::Instant::now() < drain_deadline {
        if let Ok((method, params)) =
            tokio::time::timeout(Duration::from_millis(100), node.next_frame()).await
        {
            assert_ne!(
                method, "gate.abortLanding",
                "the legacy resolution aborted the parked land: {params}"
            );
        }
    }
    // The whole row is unchanged and the blocked verify still has not moved.
    let after = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        after, parked,
        "the uncertain Landing row must be unchanged: {after}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(900), hook.wait_arrived())
            .await
            .is_err(),
        "the parked lane must not dispatch the verify job"
    );
    let blocked = ctx.hub.test_get_gate_job(&blocker).await.unwrap().unwrap();
    assert_eq!(blocked["state"], json!("queued"), "{blocked}");

    // 2) Carrier offline, no probe authority at all -> the same 409, no abort,
    // row still byte-identical.
    node.disconnect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while ctx.hub.test_node_session(&ctx.host).await.is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "host never went offline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, reply) = ctx
        .send("POST", &resolve_path, &ctx.human, None, Some(request()))
        .await;
    assert_eq!(
        status, 409,
        "missing probe authority must still be refused on the legacy route: {reply}"
    );
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or("")
            .contains("resolve-evidence"),
        "{reply}"
    );
    let after_offline = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        after_offline, parked,
        "the uncertain Landing row must be unchanged while offline: {after_offline}"
    );
    assert_eq!(after_offline["hostId"], json!(ctx.host), "{after_offline}");
    assert_eq!(after_offline["laneId"], json!("lane1"), "{after_offline}");
}

/// r27 item 2: evidence uncertainty arriving WHILE a legacy `/resolve`
/// probe is outstanding must not cause an abort. The request starts on a
/// PLAIN recovery-expired Landing (no condition); the final probe is held at
/// its test barrier; a delayed landed verdict whose evidence persist fails
/// records the evidence-deferral condition; the probe is then released with an
/// Unknown answer. The fresh pre-abort validation must see the new condition
/// and answer 409 — serialized against the deferral transition by the per-job
/// decision guard — with NO `gate.abortLanding` frame and a byte-identical row.
#[tokio::test]
async fn an_evidence_deferral_during_the_legacy_resolve_probe_never_aborts() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;

    // Grant a LAND and age it into a PLAIN recovery-expired Landing (no
    // evidence condition yet).
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx
        .hub
        .test_node_session(&ctx.host)
        .await
        .expect("carrier live");
    let merge = "d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    let epoch = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["landingSessionEpoch"]
        .as_str()
        .expect("grant epoch")
        .to_owned();

    node.set_resolve_reply(json!({ "jobId": job_id, "status": "unknown" }))
        .await;
    ctx.hub
        .test_set_landing_at(&job_id, "2000-01-01T00:00:00Z")
        .await
        .unwrap();
    ctx.hub.test_force_landing_reconcile(&ctx.host).await;
    let parked_plain = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(parked_plain["state"], json!("landing"), "{parked_plain}");
    assert_eq!(
        parked_plain["landingOutcomeUnknown"],
        json!(true),
        "{parked_plain}"
    );
    assert!(
        parked_plain["evidenceUncertain"].is_null(),
        "the resolve starts WITHOUT an evidence condition: {parked_plain}"
    );

    // Arm the probe barrier and fire the legacy resolve; it parks while the
    // final probe is outstanding.
    let probe_barrier = ctx.store().gate_resolve_probe_barrier();
    probe_barrier.arm();
    let resolve_path = format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project);
    let body = json!({
        "resolution": "failed",
        "expectedAttempts": 1,
        "expectedSessionEpoch": epoch,
        "expectedMergeSha": merge,
        "rationale": "operator resolve while a delayed verdict is still arriving",
    });
    let resolve_call = tokio::spawn({
        let http = ctx.http.clone();
        let base = ctx.base();
        let human = ctx.human.clone();
        async move {
            let response = http
                .post(format!("{base}{resolve_path}"))
                .bearer_auth(&human)
                .json(&body)
                .send()
                .await
                .expect("resolve response");
            let status = response.status();
            let value = response.json().await.unwrap_or(json!(null));
            (status, value)
        }
    });
    probe_barrier.wait_parked(1).await;

    // While the probe is held: the delayed landed verdict arrives with
    // retained logs, and its evidence persist FAILS — the Hub records the
    // evidence-deferral condition without moving the Landing.
    let landed_result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "landed",
        "mergeSha": merge,
        "runLog": {
            "step": "*", "kind": "kept", "headline": "landed; log retained by --keep-logs",
            "summary": [], "tail": ["Finished land successfully"],
            "capturedLines": 1, "truncated": false, "attempts": 1,
        },
    });
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, landed_result)
        .await
        .unwrap();
    ctx.store().test_clear_gate_log_insert_fault();
    let uncertain = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(uncertain["state"], json!("landing"), "{uncertain}");
    assert!(
        uncertain["evidenceUncertain"].is_object(),
        "the deferred condition is parked while the probe is held: {uncertain}"
    );

    // Release the probe with its Unknown answer: 409, no abort, row unchanged.
    probe_barrier.release_key(&job_id).await;
    let (status, reply) = tokio::time::timeout(Duration::from_secs(15), resolve_call)
        .await
        .expect("the held resolve must finish after the probe releases")
        .expect("the resolve task joins");
    assert_eq!(
        status, 409,
        "a resolve whose row picked up uncertainty mid-probe is refused: {reply}"
    );
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or("")
            .contains("resolve-evidence"),
        "the conflict directs the operator to the evidence route: {reply}"
    );

    // The held probe's resolveLanding frame may be queued; nothing after it may
    // be a gate.abortLanding.
    let drain_deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < drain_deadline {
        if let Ok((method, params)) =
            tokio::time::timeout(Duration::from_millis(100), node.next_frame()).await
        {
            assert_ne!(
                method, "gate.abortLanding",
                "the legacy resolution aborted the land after uncertainty arrived: {params}"
            );
        }
    }
    // The probe itself must have been answered (not skipped), and the row is
    // byte-identical to the parked uncertain row.
    let after = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        after, uncertain,
        "the uncertain Landing row must be unchanged by the refused resolve: {after}"
    );
    assert_eq!(after["state"], json!("landing"), "{after}");
    assert!(after["evidenceUncertain"].is_object(), "{after}");
}

/// r24 item 1 audit: the periodic Landing reconciler is an authorized settler
/// too. An evidence-uncertain Landing whose read-only probe ESTABLISHES the
/// merge absent with the land process group verified dead is requeued (or
/// failed at the cap) with the stale condition CLEARED — the fact-based
/// settlement is the same authority as a Landed probe, so the condition must
/// not dangle on the requeued row where a later attempt could never resolve it.
#[tokio::test]
async fn a_proven_absent_probe_clears_an_evidence_uncertain_landing_on_requeue() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let merge = "cccccccccccccccccccccccccccccccccccccccc";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    // A definitive landed verdict whose keepLogs evidence cannot persist
    // twice: the row stays Landing with the uncertain condition.
    let result = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "landed",
        "mergeSha": merge,
        "runLog": {
            "step": "*", "kind": "kept", "headline": "landed; log retained by --keep-logs",
            "summary": [], "tail": ["Finished land successfully"], "capturedLines": 1,
            "truncated": false, "attempts": 1,
        },
    });
    ctx.store().test_arm_gate_log_insert_fault_sticky();
    ctx.hub
        .test_apply_gate_result(&job_id, result.clone())
        .await
        .unwrap();
    ctx.hub
        .test_apply_gate_result(&job_id, result.clone())
        .await
        .unwrap();
    ctx.store().test_clear_gate_log_insert_fault();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert!(doc["evidenceUncertain"].is_object(), "{doc}");

    // The probe establishes absent with the group verified dead.
    node.set_resolve_reply(json!({
        "jobId": job_id, "status": "absent",
        "baseSha": null, "error": "merge absent and land group verified gone",
    }))
    .await;
    ctx.hub.test_force_landing_reconcile(&ctx.host).await;

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("queued"),
        "established absent (under the attempt cap) requeues: {doc}"
    );
    assert!(
        doc["evidenceUncertain"].is_null(),
        "the fact-based absent settlement clears the parked condition: {doc}"
    );
    assert!(doc["mergeSha"].is_null(), "{doc}");
    assert!(doc["landingSessionEpoch"].is_null(), "{doc}");
}

// ── 6c. A narrowed Bot token acts for the live chapter (CLI path) ─────────
#[tokio::test]
async fn narrowed_bot_token_writes_a_live_chapter_even_after_a_fence_check() {
    // caller() lets an unbound device narrow with x-remuda-instance-id; the
    // CLI sets it from REMUDA_INSTANCE_ID. The device clause must accept ANY
    // unbound device row (Human or Bot): only unbound devices can narrow, and
    // a narrowed caller acts with the chapter's initiator (§7.1).
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    let bot = ctx
        .hub
        .mint_bot_device_token("initiator-bot")
        .await
        .unwrap();

    let (status, body) = ctx
        .send(
            "POST",
            &format!("/v1/instances/{}/commands", ctx.instance),
            &bot,
            Some(&ctx.instance),
            Some(Ctx::send_body("instance.send")),
        )
        .await;
    assert_eq!(
        status, 200,
        "a narrowed Bot write on a live chapter: {status} {body}"
    );
    let (method, params) = node.next().await;
    assert_eq!(method, "instance.send");
    // A narrowed token IS an Agent caller for that chapter (§7.1 states it
    // for Human; item 6 extends the unbound-device rule to Bot): the Hub
    // stamps the chapter initiator, and the Hub-only device id never ships.
    assert_eq!(
        params["initiator"]["instanceId"],
        json!(ctx.instance),
        "the narrowed Bot acts with the chapter initiator: {params}"
    );
    assert!(params.get("initiatorDeviceId").is_none(), "{params}");

    // Fencing the chapter does not widen the Bot: without the narrow header
    // it is plain Bot (Human-equivalent) and still writes.
    ctx.fence().await;
    let (status, body) = ctx
        .send(
            "POST",
            &format!("/v1/instances/{}/commands", ctx.instance),
            &bot,
            None,
            Some(Ctx::send_body("instance.send")),
        )
        .await;
    assert_eq!(status, 200, "plain Bot work is never fenced: {body}");
}

// ── 7. Human token narrowed to a fenced chapter ───────────────────────────

#[tokio::test]
async fn human_token_narrowed_to_a_fenced_chapter_is_refused() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.fence().await;

    let (status, body) = ctx
        .send(
            "POST",
            &format!("/v1/instances/{}/commands", ctx.instance),
            &ctx.human,
            Some(&ctx.instance),
            Some(Ctx::send_body("instance.send")),
        )
        .await;
    assert_fenced(status, &body);
}

// ── 7b. Pool binding fence lands mid-handler (lease vs mutate) ────────────

#[tokio::test]
async fn fence_before_lease_admission_refuses_fenced_and_writes_no_task_or_lease() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // Deterministic seam (NOT a pre-request fence): the fence applies INSIDE
    // the next admit_node_op writer job (consumed only by admit_node_op and
    // claim_gate_job) — so create_task commits healthy first, then the
    // worktree.lease admission refuses. With a pre-request ctx.fence() the
    // request died at create_task and this test proved nothing about
    // bind_task_directory / admit_node_op / lease_refusal.
    ctx.store()
        .test_arm_fence_before_authority_check(ctx.instance.clone());
    let (status, body) = ctx.post_pool_task("fence-before-lease").await;
    assert_fenced(status, &body);
    assert!(
        ctx.task_list().await.is_empty(),
        "task row survived a fenced lease admission: {body}"
    );
    assert!(
        ctx.lease_rows().is_empty(),
        "lease row written despite a fenced admission"
    );
    // No worktree.lease reached the Node: the next frame is the sentinel.
    node.assert_next_frame_is_sentinel(&ctx).await;
}

#[tokio::test]
async fn fence_between_lease_and_bind_mutate_returns_the_lease_and_drops_the_task() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // Deterministic seam: the fence applies inside the NEXT mutate_task
    // writer job — i.e. after worktree.lease returned and before the
    // binding commits. The lease admission (its own writer) is unaffected.
    ctx.store()
        .test_arm_fence_before_task_mutate(ctx.instance.clone());
    let (status, body) = ctx.post_pool_task("fence-between-lease-and-mutate").await;
    assert_fenced(status, &body);

    // The task row was cascaded away.
    assert!(
        ctx.task_list().await.is_empty(),
        "task row survived a fenced bind mutate: {body}"
    );

    // The Hub unwound the lease internally: worktree.lease was sent, then
    // worktree.return (initiator-less Hub cleanup), and nothing else.
    let (method, lease_params) = node.next().await;
    assert_eq!(method, "worktree.lease");
    let (method, return_params) = node.next().await;
    assert_eq!(
        method, "worktree.return",
        "expected internal worktree.return"
    );
    assert!(
        return_params.get("initiator").is_none(),
        "Hub-internal cleanup carries no initiator: {return_params}"
    );
    // The Hub released its catalog claim: no task holder remains on the row
    // (the pool slot's state stays `leased` until worktree.remove confirms
    // parking — this internal unwind never sends that, by design).
    let rows = ctx.lease_rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].1.is_none(), "lease holder retained: {rows:?}");
    node.assert_next_frame_is_sentinel(&ctx).await;
    let _ = lease_params;
}

// ── 8. Body-supplied initiator/actor are ignored ──────────────────────────

#[tokio::test]
async fn body_supplied_initiator_or_actor_is_ignored_the_authenticated_one_is_stamped() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    let forger = json!({
        "instanceId":"ins_impostor","lineageId":"ins_impostor","generation":99
    });
    let mut body = Ctx::send_body("instance.send");
    body["initiator"] = forger.clone();
    body["initiatorDeviceId"] = json!("dev_impostor");
    body["actor"] = json!({"id":"prn_impostor"});
    if let Some(payload) = body["payload"].as_object_mut() {
        payload.insert("initiator".into(), forger.clone());
        payload.insert("actor".into(), json!({"type":"human"}));
    }

    let (status, response) = ctx
        .agent_post(&format!("/v1/instances/{}/commands", ctx.instance), body)
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(
        response["command"]["initiator"]["instanceId"],
        json!(ctx.instance),
        "stored initiator must come from auth, not the body: {response}"
    );

    let (method, params) = node.next().await;
    assert_eq!(method, "instance.send");
    assert_eq!(
        params["initiator"]["instanceId"],
        json!(ctx.instance),
        "forwarded initiator must be the authenticated chapter: {params}"
    );
    assert_eq!(params["initiator"]["generation"].as_i64(), Some(1));
    assert!(
        params.get("initiatorDeviceId").is_none(),
        "device id must never be forwarded: {params}"
    );
}

// ── 9. Human/Bot requests are byte-identical after a fence ────────────────

#[tokio::test]
async fn human_requests_remain_unchanged_after_a_fence_and_carry_no_initiator() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    ctx.fence().await;

    let (status, body) = ctx
        .send(
            "POST",
            &format!("/v1/instances/{}/commands", ctx.instance),
            &ctx.human,
            None,
            Some(Ctx::send_body("instance.send")),
        )
        .await;
    assert_eq!(status, 200, "human work is never fenced: {body}");
    assert!(
        body["command"].get("initiator").is_none(),
        "human command must not serialize an initiator: {body}"
    );
    let (method, params) = node.next().await;
    assert_eq!(method, "instance.send");
    assert!(params.get("initiator").is_none(), "{params}");
}

// ── 9. Fence between create admission and create command unwinds the row ─

#[tokio::test]
async fn fenced_instance_create_unwinds_the_instance_row_and_writes_nothing() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // The deterministic seam lands inside the create COMMAND writer job
    // (after insert_instance_delegated, at its check_initiator): the
    // exact post-admission F window.
    ctx.store()
        .test_arm_fence_before_queue(ctx.instance.clone());

    let (status, body) = ctx
        .agent_post(
            "/v1/instances",
            json!({"hostId":ctx.host,"kind":"claude","driver":"claude-print",
                   "permissionMode":"manual","prompt":"fenced child"}),
        )
        .await;
    assert_fenced(status, &body);

    // No instance row at all survives: the insert ran, but the fence lands
    // inside the create-command writer and the unwind deletes it.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let instance_count: i64 = db
        .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(instance_count, 1, "only the boot chapter survives: {body}");

    // No frame reached the Node: the create command never admitted.
    node.assert_next_frame_is_sentinel(&ctx).await;
}

// ── 9b. A fenced CONTINUITY child also unwinds its lineage (seat) row ─────

#[tokio::test]
async fn fenced_continuity_child_create_unwinds_its_lineage_row_as_well() {
    let (ctx, _node) = Ctx::boot().await.unwrap();

    // The boot coordinator is itself a continuity chapter (it holds grants),
    // so it already owns exactly one lineage row.
    let lineages_before = {
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.query_row::<i64, _, _>("SELECT COUNT(*) FROM lineages", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(lineages_before, 1);

    // A grant-bearing child creates a SECOND lineage row at insert time; the
    // `land` grant (held by the parent) has no one-per-* uniqueness rule.
    ctx.store()
        .test_arm_fence_before_queue(ctx.instance.clone());
    let (status, body) = ctx
        .agent_post(
            "/v1/instances",
            json!({"hostId":ctx.host,"projectId":ctx.project,"kind":"claude",
                   "driver":"claude-print","grants":["land"],
                   "permissionMode":"manual","prompt":"continuity child"}),
        )
        .await;
    assert_fenced(status, &body);

    // Both the orphaned chapter AND its lineage reservation are gone; the
    // parent's lineage row is untouched.
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let instances: i64 = db
        .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(instances, 1, "only the boot chapter survives: {body}");
    let lineages_after: i64 = db
        .query_row("SELECT COUNT(*) FROM lineages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        lineages_after, 1,
        "the purged child's lineage seat row must be unwound too"
    );
}

// ── 9c. A fenced project PATCH is atomic: no half-written doc/route ───────

#[tokio::test]
async fn fenced_project_patch_writes_neither_the_doc_nor_the_route_override() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    assert!(
        ctx.store()
            .get_project_route_override(ctx.project.clone())
            .await
            .unwrap()
            .is_none(),
        "no route override before the PATCH"
    );

    // The fence lands INSIDE the (single) PATCH writer job.
    ctx.store()
        .test_arm_fence_before_project_patch(ctx.instance.clone());

    // One PATCH carrying BOTH a doc field and the side-table field.
    let (status, body) = ctx
        .agent_patch(
            &format!("/v1/projects/{}", ctx.project),
            json!({"name":"half-written-after-fence","apiVia":"self"}),
        )
        .await;
    assert_fenced(status, &body);

    // Neither half committed.
    let project: Value = ctx
        .http
        .get(format!("{}/v1/projects/{}", ctx.base(), ctx.project))
        .bearer_auth(&ctx.human)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        project["name"],
        json!("initiator-project"),
        "the project doc half-wrote despite the fence: {project}"
    );
    assert!(
        ctx.store()
            .get_project_route_override(ctx.project.clone())
            .await
            .unwrap()
            .is_none(),
        "the route override half-wrote in a separate writer job"
    );
}

// ── 9d. A fence in mark_forward_intent purges the requested chapter instead
//      of letting expire_stale_requested settle a never-Node chapter (OA6) ──

#[tokio::test]
async fn fenced_forward_intent_purges_the_requested_instance_and_its_command() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // The fence lands INSIDE the mark_forward_intent writer job: the create
    // command was already admitted and the instance row holds its slot, but
    // the stamped-initiator re-check refuses before any frame is sent.
    ctx.store()
        .test_arm_fence_before_forward_intent(ctx.instance.clone());

    let (status, body) = ctx
        .agent_post(
            "/v1/instances",
            json!({"hostId":ctx.host,"kind":"claude","driver":"claude-print",
                   "permissionMode":"manual","prompt":"fenced at forward intent"}),
        )
        .await;
    assert_fenced(status, &body);

    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let instance_count: i64 = db
        .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(instance_count, 1, "only the boot chapter survives: {body}");
    // The queued create behind the purged row must be gone too
    // (purge_requested_instance deletes commands by instance_id).
    let orphan_commands: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM commands
              WHERE instance_id IS NOT NULL
                AND instance_id NOT IN (SELECT id FROM instances)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        orphan_commands, 0,
        "the queued create command must be purged with the requested row"
    );
    // The held slot is derived from the instances row (INSERT_SLOT_COUNT_SQL),
    // so instance_count == 1 also proves the requested slot was released.
    // No frame reached the Node: the forward-intent writer refused first.
    node.assert_next_frame_is_sentinel(&ctx).await;
}

// ── 9e. Same fence for a CONTINUITY child: its `starting` lineage seat is
//      purged as well, not left holding the lineage after the chapter ──────

#[tokio::test]
async fn fenced_forward_intent_on_a_continuity_child_purges_its_lineage_seat() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    let lineages_before: i64 = {
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.query_row("SELECT COUNT(*) FROM lineages", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(
        lineages_before, 1,
        "the boot coordinator owns the one lineage"
    );

    // A grant-bearing child creates a SECOND lineage row at insert time; the
    // fence then lands in mark_forward_intent before a frame is sent.
    ctx.store()
        .test_arm_fence_before_forward_intent(ctx.instance.clone());
    let (status, body) = ctx
        .agent_post(
            "/v1/instances",
            json!({"hostId":ctx.host,"projectId":ctx.project,"kind":"claude",
                   "driver":"claude-print","grants":["land"],
                   "permissionMode":"manual","prompt":"continuity child fenced at intent"}),
        )
        .await;
    assert_fenced(status, &body);

    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    let instances: i64 = db
        .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(instances, 1, "only the boot chapter survives: {body}");
    let lineages_after: i64 = db
        .query_row("SELECT COUNT(*) FROM lineages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        lineages_after, 1,
        "the child's `starting` lineage seat must be purged, not left holding a dead chapter"
    );
    node.assert_next_frame_is_sentinel(&ctx).await;
}

// ── 10. Worker routes stay 403 for Agents; the node_ops admission boundary ─
//      still covers worker.provision at the helper/store level (ma-admission
//      owns the route opening: main-agent.md §15 task 7). ───────────────────

#[tokio::test]
async fn agent_worker_routes_are_forbidden_and_provision_admission_is_fence_checked() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    // Every worker route an Agent could name is refused by the pre-handler
    // middleware — the dispatch grant widens nothing until ma-admission lands.
    let (status, body) = ctx
        .agent_post(
            "/v1/workers/dispatch",
            json!({"projectId":ctx.project,"brief":BRIEF,"harness":"claude"}),
        )
        .await;
    assert_eq!(status, 403, "agent dispatch must stay 403: {body}");
    for path in [
        "/v1/workers",
        "/v1/workers/wkr_1",
        "/v1/workers/wkr_1/answer",
        "/v1/workers/wkr_1/stop",
        "/v1/workers/wkr_1/retire",
        "/v1/workers/wkr_1/nudge",
        "/v1/workers/wkr_1/observe",
    ] {
        let method = if path.contains('/') && path != "/v1/workers" {
            "POST"
        } else {
            "GET"
        };
        let (status, _) = ctx
            .send(method, path, &ctx.agent, None, Some(json!({})))
            .await;
        assert_eq!(status, 403, "agent {method} {path} must stay 403");
    }
    // No roster row, and the Node never saw worker.provision: a Human command
    // sentinel frame is the first (and only) frame after the refused calls.
    let workers: Value = ctx
        .http
        .get(format!("{}/v1/workers?project={}", ctx.base(), ctx.project))
        .bearer_auth(&ctx.human)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(workers["items"].as_array().unwrap().is_empty(), "{workers}");
    let (status, sentinel) = ctx
        .send(
            "POST",
            &format!("/v1/instances/{}/commands", ctx.instance),
            &ctx.human,
            None,
            Some(Ctx::send_body("instance.send")),
        )
        .await;
    assert_eq!(status, 200, "{sentinel}");
    let (method, _params) = node.next().await;
    assert_eq!(method, "instance.send", "provision leaked to the Node");

    // The §7.5 admission boundary itself stays: a worker.provision op for a
    // fenced initiator is refused at the helper/store level with nothing sent.
    let (initiator, device_id) = ctx.agent_authority();
    ctx.hub
        .test_admit_node_op(
            format!("wpr_{}", uuid::Uuid::now_v7()),
            ctx.host.clone(),
            "worker.provision",
            None,
            Some(initiator.clone()),
            Some(device_id),
        )
        .await
        .expect("a healthy provision op admits");
    ctx.fence().await;
    let err = ctx
        .hub
        .test_admit_node_op(
            format!("wpr_{}", uuid::Uuid::now_v7()),
            ctx.host.clone(),
            "worker.provision",
            None,
            Some(initiator),
            None,
        )
        .await
        .expect_err("a fenced provision op must not admit");
    assert!(
        format!("{err:#}").to_lowercase().contains("fenced"),
        "{err:#}"
    );
}

// ── 10b. node_ops: send intent precedes the wire; no host ⇒ not sent ──────

#[tokio::test]
async fn node_op_send_intent_precedes_the_call_and_a_missing_host_is_not_sent() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    let (initiator, device_id) = ctx.agent_authority();

    // Connected host, with the fake's reply HELD after frame arrival: while
    // the RPC is in flight (up to the timeout on main), the row must already
    // read `sent` — the send intent commits BEFORE nodes.call writes the
    // frame, not when the reply comes back. Asserted against the row while
    // the call is still pending.
    let op_sent = format!("nop_{}", uuid::Uuid::now_v7());
    let hold = node.hold_reply_for(op_sent.clone());
    let call_fut = ctx.hub.test_call_node_op(
        &ctx.host,
        "worker.provision",
        op_sent.clone(),
        Some(initiator.clone()),
        Some(device_id.clone()),
    );
    tokio::pin!(call_fut);
    tokio::select! {
        r = &mut call_fut => panic!("call completed while its reply was held: {r:?}"),
        _ = hold.wait_arrived() => {}
    }
    let (state, _outcome) = ctx
        .hub
        .test_node_op_state(&op_sent, &ctx.host)
        .await
        .unwrap()
        .expect("admitted op row exists");
    assert_eq!(
        state, "sent",
        "send intent must commit while the frame is on the wire"
    );
    hold.release();
    let reached = call_fut.await.unwrap();
    assert!(reached, "the connected fake Node must answer");
    let (state, _outcome) = ctx
        .hub
        .test_node_op_state(&op_sent, &ctx.host)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state, "settled");

    // A host that exists in the catalog but holds no live Node session:
    // nodes.call writes nothing and returns Ok(None) deterministically —
    // no disconnect, no timing. The row must return to `admitted`/notSent,
    // never stay `sent`, which ma-fence step 7 would read as "possibly
    // executed".
    let offline_host = format!("hst_{}", uuid::Uuid::now_v7());
    {
        // Clone the enrolled host's catalog row under a fresh id via a temp
        // table (column list follows the live migration); no Node is (or
        // ever was) connected with it.
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.execute_batch(&format!(
            "CREATE TEMP TABLE host_clone AS SELECT * FROM hosts WHERE id = '{}';
             UPDATE host_clone
                SET id = '{offline_host}',
                    token_hash = 'offline-{offline_host}',
                    token_prefix = 'pfxoffline{tag}';
             INSERT INTO hosts SELECT * FROM host_clone;",
            ctx.host,
            tag = uuid::Uuid::now_v7().simple()
        ))
        .unwrap();
    }
    let op_unsent = format!("nop_{}", uuid::Uuid::now_v7());
    let reached = ctx
        .hub
        .test_call_node_op(
            &offline_host,
            "worker.provision",
            op_unsent.clone(),
            Some(initiator),
            Some(device_id),
        )
        .await
        .unwrap();
    assert!(!reached, "no frame can be written without a live session");
    let (state, outcome) = ctx
        .hub
        .test_node_op_state(&op_unsent, &offline_host)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state, "admitted", "an unwritten op must not read sent");
    assert!(
        outcome.unwrap().contains("notSent"),
        "outcome must record notSent"
    );
}

// ── 11. Direct store semantics for the stamped-row intent re-check ────────

#[tokio::test]
async fn mark_forward_intent_rechecks_the_stamp_on_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let (store, host_id) =
        remuda_hub::store_test_support::open_with_host(dir.path(), "initiator-intent")
            .await
            .unwrap();

    // An instance with lineage + launch device rows.
    let instance = store
        .insert_instance(
            host_id,
            None,
            "claude".into(),
            "claude-print".into(),
            None,
            json!({}),
        )
        .await
        .unwrap();
    let initiator = remuda_protocol::Initiator {
        instance_id: instance.instance_id.clone(),
        lineage_id: instance.lineage_id.clone(),
        generation: instance.generation,
    };
    let launch_device = store
        .insert_device_as(
            "launch".into(),
            "hash-a".into(),
            "pfx-a".into(),
            "agent".into(),
            Some(instance.instance_id.clone()),
        )
        .await
        .unwrap()
        .id;

    // A held row queued while healthy: marking intent works.
    let (held, created) = store
        .queue_command(
            None,
            Some(instance.instance_id.clone()),
            instance.host_id.clone(),
            "instance.send".into(),
            json!({}),
            None,
            None,
            Some(initiator.clone()),
            Some(launch_device.clone()),
        )
        .await
        .unwrap();
    assert!(created);
    store
        // No hello has adopted this host's link: its durable generation is 0.
        .mark_forward_intent(held.command_id.clone(), 0)
        .await
        .unwrap();

    // A second held row: once its STAMPED device row is gone (as F removes
    // predecessor devices), the intent re-check refuses Fenced — even though
    // the row itself is unchanged. The queue-time check and the intent check
    // read the same authority, so queue first with a fresh device row.
    let second_device = store
        .insert_device_as(
            "mcp".into(),
            "hash-b".into(),
            "pfx-b".into(),
            "agent".into(),
            Some(instance.instance_id.clone()),
        )
        .await
        .unwrap()
        .id;
    let (second, created) = store
        .queue_command(
            None,
            Some(instance.instance_id.clone()),
            instance.host_id.clone(),
            "instance.send".into(),
            json!({}),
            None,
            None,
            Some(initiator),
            Some(second_device.clone()),
        )
        .await
        .unwrap();
    assert!(created);
    // The first row's stamp used the launch device; deleting the SECOND
    // device must not affect the first (already marked anyway). Delete the
    // second row, then the second row's intent claim is refused Fenced.
    store.test_delete_device(second_device).await.unwrap();
    let err = store
        .mark_forward_intent(second.command_id.clone(), 0)
        .await
        .unwrap_err();
    assert!(
        matches!(err, remuda_hub::store_test_support::StoreError::Fenced),
        "stamped-row intent re-check must be Fenced, got {err:?}"
    );

    // Hub-internal successor work (initiator, no device id) skips the device
    // clause: deleting the launch device does not block it.
    store.test_delete_device(launch_device).await.unwrap();
    let (successor, _) = store
        .queue_command(
            None,
            Some(instance.instance_id.clone()),
            instance.host_id,
            "instance.close".into(),
            json!({}),
            None,
            None,
            // Successor initiator: same lineage, NEXT generation is what F
            // would write; use the current row's live generation here since
            // the test fence did not bump it — the point is the missing device
            // clause alone.
            Some(remuda_protocol::Initiator {
                instance_id: instance.instance_id,
                lineage_id: instance.lineage_id,
                generation: instance.generation,
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(successor.initiator_device_id, None);
}

/// A verdict carries the attempt generation it belongs to. After a
/// base-moved requeue the OLD attempt's delayed verdict (generation 1) must
/// be dropped when the row is already a newly Running attempt 2 — accepting
/// it would requeue attempt 2, burn a retry and cause a later lane-busy.
/// No SQL: the generation gate is in the normal pre-read.
#[tokio::test]
async fn a_stale_attempts_verdict_is_dropped_after_a_requeue() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let job_id = queued_land_job(&ctx).await;
    // Claim attempt 1, then a REAL base-moved verdict requeues it and the
    // scheduler claims attempt 2 (no SQL writes to gate state).
    let _ = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap();
    genuine_requeue_to_attempt_two(&ctx, &job_id).await;

    // The OLD attempt's verdict arrives late. It must be dropped entirely
    // (no state change, no log reference) rather than applied to attempt 2.
    let stale = json!({
        "jobId": job_id,
        "generation": 1,
        "status": "base-moved",
        "currentMainSha": "0123456789abcdef0123456789abcdef01234567",
    });
    ctx.hub
        .test_apply_gate_result(&job_id, stale)
        .await
        .unwrap();

    // A CURRENT-generation (2) failed verdict on attempt 2 is applied
    // normally, proving the drop was generation-specific, not a dead path.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "generation": 2,
                "status": "failed",
                "error": "attempt 2 failed",
                "reason": "attempt 2 failed",
            }),
        )
        .await
        .unwrap();
    let final_doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(final_doc["state"], json!("failed"), "{final_doc}");
    assert_eq!(
        final_doc["error"],
        json!("attempt 2 failed"),
        "the stale verdict must not have applied: {final_doc}"
    );
}

/// r11 item 4 (r12 item 9): attempt 1's gate.run RPC failure must not fail the
/// running attempt 2 after a genuine base-moved requeue — driven through the
/// REAL dispatch error callback (the fake Node answers the gate.run RPC with
/// a JSON-RPC error), not a store-copy test seam.
#[tokio::test]
async fn an_rpc_failure_for_attempt_one_does_not_fail_running_attempt_two() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;

    // Attempt 1's gate.run is parked at the Node (its RPC reply withheld).
    let hook1 = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    let (status, enqueued) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/stale-rpc","mode":"land"}),
        )
        .await;
    assert_eq!(status, 200, "{enqueued}");
    let job_id = enqueued["id"].as_str().unwrap().to_owned();
    hook1.wait_arrived().await;
    assert_eq!(
        ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["state"],
        json!("running"),
        "attempt 1 dispatched and parked"
    );

    // Attempt 1 requeues via its real finished-event base-moved verdict (the
    // gate.run RPC itself is still parked — the dual-path ordering).
    node.send_raw(json!({
        "jsonrpc": "2.0", "method": "gate.event",
        "params": {
            "jobId": job_id, "generation": 1, "kind": "finished",
            "result": {
                "jobId": job_id, "generation": 1, "status": "base-moved",
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567",
            },
        },
    }));

    // The scheduler claims attempt 2 and dispatches a fresh gate.run. Script
    // it (parked, will answer with an RPC error) before attempt 2's frame.
    let hook2 = node
        .arm_gate_run(json!({ "jobId": "placeholder", "status": "failed" }))
        .await;
    hook2.wait_arrived().await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["attempts"], json!(2), "attempt 2 dispatched: {doc}");
    assert_eq!(doc["state"], json!("running"), "{doc}");

    // Attempt 1's parked gate.run now answers with a JSON-RPC error. It is the
    // stale dispatch's callback; its failure must NOT fail attempt 2. The
    // latching is the PRODUCTION decision: the gate.run reply runs fail_rpc's
    // writer job, and the test waits for that handler's completion latch, then
    // reads the row once. No stability window and no sleeps.
    hook1.reply_error();
    hook1.release_and_wait_proceeded().await;
    let stale_barrier = ctx.store().gate_apply_barrier();
    let rpc_before = stale_barrier.rpc_completed_count();
    stale_barrier.wait_rpc_completed(rpc_before + 1).await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("running"),
        "stale attempt-1 RPC failure must not fail attempt 2: {doc}"
    );

    // The CURRENT (attempt 2) gate.run RPC error does fail it.
    hook2.reply_error();
    hook2.release_and_wait_proceeded().await;
    for _ in 0..100 {
        let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
        if doc["state"] == json!("failed") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("current-attempt gate.run RPC error must fail the job");
}

/// r11 item 5: a step event from the OLD attempt must not overwrite step
/// evidence on the current attempt after a requeue.
#[tokio::test]
async fn a_stale_attempts_step_event_does_not_overwrite_the_current_steps() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    let job_id = queued_land_job(&ctx).await;
    // claim 1, then a REAL base-moved verdict requeues and claims attempt 2.
    let _ = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap();
    genuine_requeue_to_attempt_two(&ctx, &job_id).await;

    // A current-attempt (2) step lands; latch on the production event
    // completion instead of polling with sleeps.
    let event_barrier = ctx.store().gate_apply_barrier();
    let events_before = event_barrier.event_completed_count();
    node.send_raw(json!({
        "jsonrpc": "2.0",
        "method": "gate.event",
        "params": {
            "jobId": job_id,
            "generation": 2,
            "kind": "step",
            "step": { "name": "cargo-test", "status": "running", "durationMs": 0 },
        },
    }));
    event_barrier.wait_event_completed(events_before + 1).await;
    let applied = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        applied["steps"].as_array().map(|a| a.len()).unwrap_or(0),
        1,
        "attempt-2 step applied: {applied}"
    );

    // A stale attempt-1 step for the SAME step name must be ignored. The
    // rejection is latched on the production event-completion signal, not a
    // stability window: send two stale steps (would set status ok/9999ms),
    // wait for BOTH event handlers to fully return, then read the row once.
    for _ in 0..2 {
        node.send_raw(json!({
            "jsonrpc": "2.0",
            "method": "gate.event",
            "params": {
                "jobId": job_id,
                "generation": 1,
                "kind": "step",
                "step": { "name": "cargo-test", "status": "ok", "durationMs": 9999 },
            },
        }));
    }
    event_barrier.wait_event_completed(events_before + 2).await;
    // The event writers decided before this read: the step must still be
    // attempt 2's (running, 0ms); a stale step that applied would show
    // ok/9999.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    let step = &doc["steps"][0];
    assert_eq!(step["name"], json!("cargo-test"), "{doc}");
    assert_eq!(
        step["status"],
        json!("running"),
        "stale attempt-1 step must not overwrite attempt-2 step: {doc}"
    );
    assert_eq!(step["durationMs"], json!(0), "{doc}");
}

/// r12 item 8: after a GENUINE base-moved requeue claims attempt 2, attempt
/// 1's late evidence (its runLog arriving with the stale base-moved verdict)
/// must not replace attempt 2's log object. The generation check lives
/// INSIDE the evidence transaction, so this drives both verdicts through the
/// real apply path and then reads the stored object bytes.
#[tokio::test]
async fn attempt_one_log_cannot_replace_attempt_two_after_a_genuine_requeue() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let job_id = queued_land_job(&ctx).await;
    let _ = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap();
    genuine_requeue_to_attempt_two(&ctx, &job_id).await;

    // The CURRENT attempt 2 fails with ITS evidence.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id, "generation": 2, "status": "failed",
                "error": "attempt 2 failed", "reason": "attempt 2 failed",
                "runLog": {
                    "step": "cargo-test", "kind": "failed",
                    "headline": "ATTEMPT-TWO-EVIDENCE",
                    "summary": [], "tail": [], "capturedLines": 1, "truncated": false,
                },
            }),
        )
        .await
        .unwrap();
    let failed = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(failed["state"], json!("failed"), "{failed}");
    let object_id = failed["logObjectId"]
        .as_str()
        .expect("attempt-2 logObjectId")
        .to_owned();

    // Attempt 1's late base-moved verdict arrives carrying DIFFERENT evidence.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id, "generation": 1, "status": "base-moved",
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567",
                "runLog": {
                    "step": "cargo-test", "kind": "failed",
                    "headline": "ATTEMPT-ONE-EVIDENCE",
                    "summary": [], "tail": [], "capturedLines": 1, "truncated": false,
                },
            }),
        )
        .await
        .unwrap();

    // The stored object is still attempt 2's; the stale verdict's evidence
    // was gated out inside the evidence transaction.
    let (status, stored) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{stored}");
    assert_eq!(
        stored["log"]["headline"],
        json!("ATTEMPT-TWO-EVIDENCE"),
        "stale attempt-1 evidence replaced attempt 2's log: {stored}"
    );

    // And the production evidence transaction itself refuses a generation-1
    // write when the row is already attempt 2 (the pre-read→insert race),
    // leaving attempt 2's object in place.
    let stale_bytes = br#"{"headline":"ATTEMPT-ONE-EVIDENCE"}"#.to_vec();
    let stale_digest = "stale-digest".to_owned();
    let gated = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job_id,
            stale_bytes,
            stale_digest,
            "dev",
            1,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        gated, None,
        "a generation-1 evidence write must be gated out"
    );
    let (status, after) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{object_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{after}");
    assert_eq!(
        after["log"]["headline"],
        json!("ATTEMPT-TWO-EVIDENCE"),
        "the gated transaction replaced attempt 2's log: {after}"
    );
}

/// r14 item 2 (R2), probe half: a read-only landing probe carries the exact
/// (attempts, merge, session) it was taken against. After the row is requeued
/// and a NEWER attempt is granted (new generation, new granted merge on a new
/// carrier session), the stale probe result must settle nothing. Only the probe
/// for the CURRENT granted tuple may settle the row.
#[tokio::test]
async fn a_probe_for_an_older_attempt_never_settles_a_newer_landing() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;

    // Attempt 1: enqueue, claim, and take the Running → Landing grant.
    let job_id = queued_land_job(&ctx).await;
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("attempt 1 claimed");
    assert_eq!(claimed["state"], json!("running"));
    let merge1 = "1111111111111111111111111111111111111111";
    let session1: i64 = 0x1_0000_0001;
    let grant1 = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge1, session1 as u64)
        .await
        .unwrap();
    assert_eq!(grant1["granted"], json!(true), "{grant1}");
    assert_eq!(
        ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap()["state"],
        json!("landing")
    );

    // Attempt 1 is requeued (a genuine base-moved verdict), then claimed and
    // granted AGAIN as attempt 2 with a different merge and carrier session.
    genuine_requeue_to_attempt_two(&ctx, &job_id).await;
    let merge2 = "2222222222222222222222222222222222222222";
    let session2: i64 = 0x2_0000_0002;
    let grant2 = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge2, session2 as u64)
        .await
        .unwrap();
    assert_eq!(grant2["granted"], json!(true), "{grant2}");
    let live = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(live["state"], json!("landing"), "{live}");
    assert_eq!(live["attempts"], json!(2), "{live}");

    // The STALE probe — attempt 1's tuple, even reporting landed — settles
    // nothing: the row is a newer Landing now.
    let stale = ctx
        .hub
        .test_settle_landing_probe(
            &job_id,
            1,
            merge1.to_owned(),
            Some(session1),
            &ctx.host,
            true,
        )
        .await
        .unwrap();
    assert!(
        stale.is_none(),
        "an old generation probe must not settle a newer Landing: {stale:?}"
    );
    let still = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(still["state"], json!("landing"), "{still}");
    assert_eq!(still["attempts"], json!(2), "{still}");
    assert_eq!(still["mergeSha"], json!(merge2), "{still}");

    // The CURRENT attempt's probe settles landed.
    let current = ctx
        .hub
        .test_settle_landing_probe(
            &job_id,
            2,
            merge2.to_owned(),
            Some(session2),
            &ctx.host,
            true,
        )
        .await
        .unwrap()
        .expect("the current granted attempt probe settles");
    assert_eq!(current["state"], json!("landed"), "{current}");
    assert_eq!(current["mergeSha"], json!(merge2), "{current}");
}

/// r17 item 2: a `base-moved` verdict is the CAS-LOSS requeue, and like every
/// other Landing transition it must name the EXACT granted merge (the
/// production Node preserves the pinned merge on its CAS-loss branch). With
/// the merge present it requeues and clears mergeSha for a fresh verify; a
/// base-moved verdict missing the granted merge is a separate regression
/// below and must NOT discard the Landing.
#[tokio::test]
async fn a_base_moved_landing_verdict_naming_the_granted_merge_requeues() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let merge = "1212121212121212121212121212121212121212";
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    // CAS loss carrying the granted merge (the Node retains the pinned sha).
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "generation": 1,
                "status": "base-moved",
                "mergeSha": merge,
                "currentMainSha": "fedcba9876543210fedcba9876543210fedcba98",
            }),
        )
        .await
        .unwrap();
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("queued"), "{doc}");
    assert_eq!(
        doc["attempts"],
        json!(1),
        "the attempt count bumps on re-claim: {doc}"
    );
    assert!(
        doc["mergeSha"].is_null(),
        "the old granted merge is cleared: {doc}"
    );
}

/// r17 item 2: a `base-moved` verdict that does NOT name the granted merge is
/// a mismatched outcome and must be dropped, leaving the Landing intact — the
/// granted merge is required for EVERY Landing transition.
#[tokio::test]
async fn a_base_moved_landing_verdict_without_the_granted_merge_is_dropped() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let merge = "3434343434343434343434343434343434343434";
    let other = "5656565656565656565656565656565656565656";
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    for verdict_merge in [None, Some(other)] {
        ctx.hub
            .test_apply_gate_result(
                &job_id,
                json!({
                    "jobId": job_id,
                    "generation": 1,
                    "status": "base-moved",
                    "mergeSha": verdict_merge,
                    "currentMainSha": "fedcba9876543210fedcba9876543210fedcba98",
                }),
            )
            .await
            .unwrap();
        let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
        assert_eq!(
            doc["state"],
            json!("landing"),
            "a merge-less/wrong-merge base-moved must not discard the Landing: {doc}"
        );
        assert_eq!(
            doc["mergeSha"],
            json!(merge),
            "the granted merge is retained: {doc}"
        );
    }
}

/// r15 item 3 (R2): a carrier TAKEOVER landing in the window between a
/// Landing verdict's current-session pre-check and its writer commit is
/// serialized WITH the writer. While the verdict is parked the replacement
/// hello blocks on the per-host authority guard (the host stays on session A);
/// the verdict commits while A is still the live session, then the takeover
/// completes exactly once. The old session can never settle the row AFTER it
/// was superseded.
#[tokio::test]
async fn a_takeover_between_the_precheck_and_the_writer_is_serialized() {
    let (ctx, node_a) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("attempt 1 claimed");
    assert_eq!(claimed["state"], json!("running"));
    let session_a = ctx
        .hub
        .test_node_session(&ctx.host)
        .await
        .expect("node A is live");
    let merge = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session_a)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    let barrier = ctx.store().gate_apply_barrier();
    barrier.arm();
    // A's terminal `landed` verdict rides A's authenticated session.
    node_a.send_raw(json!({
        "jsonrpc": "2.0", "method": "gate.event",
        "params": {
            "jobId": job_id, "generation": 1, "kind": "finished",
            "result": {
                "jobId": job_id, "generation": 1, "status": "landed",
                "mergeSha": merge,
            },
        },
    }));
    barrier.wait_parked(1).await;

    // Replacement carrier connects while the verdict writer holds the guard.
    let reconnect = Box::pin(FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace));
    let contended = std::pin::pin!(ctx.hub.test_wait_node_authority_contended(&ctx.host));
    match tokio::time::timeout(
        Duration::from_secs(10),
        futures::future::select(reconnect, contended),
    )
    .await
    .expect("the replacement hello never reached the authority guard")
    {
        futures::future::Either::Right(((), _)) => {}
        futures::future::Either::Left(_) => {
            panic!("the takeover completed before the parked writer");
        }
    }
    // The host is still on A while the writer is parked.
    assert_eq!(
        ctx.hub.test_node_session(&ctx.host).await,
        Some(session_a),
        "the replacement session must not register while the writer is parked"
    );

    barrier.release();
    barrier.wait_completed(1).await;
    // The verdict settled exactly once, while A was still the CURRENT session.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landed"), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge), "{doc}");
}

/// r15 item 3 (R2), evidence/commit predicate half: while the row is Landing,
/// evidence and probe writers validate host AND authenticated session at
/// commit. A NULL source session, a different session, or a different host all
/// refuse; the exact granted (host, session) tuple commits.
#[tokio::test]
async fn landing_evidence_and_probe_writers_validate_host_and_session_at_commit() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session_a = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let merge = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session_a)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    // Evidence with NO authenticated session: refused for a Landing row
    // (previously the session fence was skipped when the source was null).
    let none_session = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job_id,
            br#"{"headline":"NO-SESSION"}"#.to_vec(),
            "digest-none".into(),
            "dev",
            1,
            None,
            Some(&ctx.host),
            Some(merge),
        )
        .await
        .unwrap();
    assert_eq!(
        none_session, None,
        "Landing evidence without a session refuses"
    );

    // Evidence on the granted host but a DIFFERENT session: refused.
    let other_session = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job_id,
            br#"{"headline":"OTHER-SESSION"}"#.to_vec(),
            "digest-other".into(),
            "dev",
            1,
            Some(session_a.wrapping_add(999)),
            Some(&ctx.host),
            Some(merge),
        )
        .await
        .unwrap();
    assert_eq!(other_session, None, "evidence from another session refuses");

    // Evidence on a different HOST id with the right session number: refused.
    let other_host = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job_id,
            br#"{"headline":"OTHER-HOST"}"#.to_vec(),
            "digest-host".into(),
            "dev",
            1,
            Some(session_a),
            Some("hst_different_host_id"),
            Some(merge),
        )
        .await
        .unwrap();
    assert_eq!(other_host, None, "evidence with a mismatched host refuses");

    // The exact (host, session) tuple commits.
    let committed = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job_id,
            br#"{"headline":"GRANTED"}"#.to_vec(),
            "digest-granted".into(),
            "dev",
            1,
            Some(session_a),
            Some(&ctx.host),
            Some(merge),
        )
        .await
        .unwrap()
        .expect("the granted host/session evidence commits");

    // A probe settle carrying a different responder host refuses, even with
    // the right (attempts, merge, session).
    let wrong_host = ctx
        .hub
        .test_settle_landing_probe(
            &job_id,
            1,
            merge.to_owned(),
            Some(session_a as i64),
            "hst_different_host_id",
            true,
        )
        .await
        .unwrap();
    assert!(
        wrong_host.is_none(),
        "a probe from another host must not settle the Landing"
    );
    let still = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(still["state"], json!("landing"), "{still}");

    // The matching tuple settles landed.
    let settled = ctx
        .hub
        .test_settle_landing_probe(
            &job_id,
            1,
            merge.to_owned(),
            Some(session_a as i64),
            &ctx.host,
            true,
        )
        .await
        .unwrap()
        .expect("the granted tuple probe settles");
    assert_eq!(settled["state"], json!("landed"), "{settled}");
    let _ = committed;
}

/// r16 item 5: the GRANTED MERGE is part of the identity bound in all three
/// Landing writers.
/// 1. grant idempotence: a re-grant for the same attempt/session but a
///    DIFFERENT merge is refused and the recorded merge is untouched;
/// r17 item 7: the grant session is a packed (epoch<<32 | counter) stored as a
/// POSITIVE signed i64. The maximum positive value (epoch 2^31-1, counter
/// 0xFFFFFFFF == i64::MAX) grants and persists verbatim; a session whose epoch
/// is 2^31 packs negative and is refused rather than collapsed to 0.
#[tokio::test]
async fn the_max_positive_i64_session_grants_but_a_2_31_epoch_is_refused() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let merge = "acacacacacacacacacacacacacacacacacacacac";
    let max_positive: u64 = i64::MAX as u64;
    assert_eq!(max_positive >> 32, (1u64 << 31) - 1);

    let job = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let grant = ctx
        .hub
        .test_gate_land_grant(&job, &ctx.host, merge, max_positive)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    assert_eq!(
        ctx.hub.test_gate_dispatch_session(&job).await.unwrap(),
        Some(max_positive as i64),
        "the max positive session persists, not collapsed to 0"
    );

    let job2 = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job2, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let overflow: u64 = 1u64 << 63; // epoch 2^31, counter 0 → negative i64
    let refused = ctx
        .hub
        .test_gate_land_grant(&job2, &ctx.host, merge, overflow)
        .await;
    assert!(
        refused.is_err(),
        "a 2^31-epoch session (negative i64) must not be granted: {refused:?}"
    );

    // r18 item 8: the refusal must come from the PRODUCTION STORE grant, not
    // merely the RunningHub wrapper's u64→i64 prevalidation. Call the store
    // grant directly on a fresh claimed job and require it to fail too; it must
    // not persist as session 0.
    let job3 = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job3, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let raw_refused = ctx
        .hub
        .test_store_gate_land_grant_raw(&job3, &ctx.host, merge, overflow)
        .await;
    assert!(
        raw_refused.is_err(),
        "the STORE grant must itself reject a non-positive-i64 session (no wrapper): \
         {raw_refused:?}"
    );
    assert_eq!(
        ctx.hub.test_gate_dispatch_session(&job3).await.unwrap(),
        None,
        "a refused store grant must not collapse the session to 0/NULL"
    );
    // Dispatch + stale-session recovery around the LAST representable epoch:
    // the max-positive session round-trips through the grant and is readable.
    assert_eq!(
        ctx.hub.test_gate_dispatch_session(&job).await.unwrap(),
        Some(max_positive as i64),
        "dispatch/recovery sees the last representable session"
    );
}

/// r18 item 7: a winning landed verdict must reference ITS OWN evidence. The
/// r19 item 4: each verdict gets an immutable content-addressed object, so a
/// concurrent same-grant result persisting different bytes can never overwrite
/// this verdict's evidence. The gate-apply barrier parks AFTER persist and
/// BEFORE the settlement writer, so we deterministically interleave: A
/// persists its object, B persists different bytes, release — A settles
/// landed serving its OWN bytes; B's object is stored but unbound and B's
/// later arrival stands down. An identical dual delivery (same digest)
/// resolves to the same object id.
#[tokio::test]
async fn a_landed_verdict_cannot_reference_another_results_evidence() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let merge = "9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b";
    ctx.hub
        .test_gate_land_grant(&job, &ctx.host, merge, session)
        .await
        .unwrap();

    let run_log = |headline: &'static str| {
        json!({
            "step": "*", "kind": "failed", "headline": headline,
            "summary": [], "tail": [], "capturedLines": 1,
            "truncated": false, "attempts": 1,
        })
    };

    // r19 item 4: evidence objects are IMMUTABLE and per result. Park A after
    // it persisted its own object; a concurrent same-grant result B persists
    // DIFFERENT bytes. They resolve to distinct content-addressed objects, so
    // A settles landed bound to ITS OWN evidence even with B interleaved.
    let barrier = ctx.store().gate_after_persist_barrier();
    barrier.arm();
    let job_a = job.clone();
    let apply_a = tokio::spawn({
        let state = ctx.hub.test_state().clone();
        async move {
            remuda_hub::RunningHub::test_apply_gate_result_static(
                &state,
                &job_a,
                json!({
                    "jobId": job_a, "generation": 1, "status": "landed",
                    "mergeSha": merge, "runLog": run_log("EVIDENCE-A"),
                }),
            )
            .await
        }
    });
    barrier.wait_parked(1).await;

    // A concurrent same-grant handler persists DIFFERENT evidence while A is
    // parked. Under the old per-job object this deleted-and-replaced A's row;
    // now it is a separate immutable object.
    let b_id = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job,
            serde_json::to_vec(&run_log("EVIDENCE-B")).unwrap(),
            "digest-b".into(),
            "dev",
            1,
            Some(session),
            Some(&ctx.host),
            Some(merge),
        )
        .await
        .unwrap()
        .expect("B persists its own object while A is parked");
    assert!(b_id.starts_with("obj_"), "{b_id}");

    // Release A: it settles landed and binds ITS OWN object, not B's.
    barrier.release();
    apply_a.await.unwrap();
    let doc = ctx.hub.test_get_gate_job(&job).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landed"), "{doc}");
    let a_id = doc["logObjectId"]
        .as_str()
        .expect("the landed job references A's object")
        .to_owned();
    assert_ne!(a_id, b_id, "A and B own distinct per-result objects: {doc}");

    let (status, served) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{a_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{served}");
    assert_eq!(
        served["log"]["headline"],
        json!("EVIDENCE-A"),
        "the settled landed verdict serves A's own bytes: {served}"
    );
    // B's object remains stored (immutable history) but is not the job's
    // reference.
    let (status_b, served_b) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{b_id}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status_b, 200, "{served_b}");
    assert_eq!(served_b["objectId"], json!(b_id), "{served_b}");

    // B's real handler then arrives and stands down at the terminal guard;
    // the bound evidence never changes.
    ctx.hub
        .test_apply_gate_result(
            &job,
            json!({
                "jobId": job, "generation": 1, "status": "landed",
                "mergeSha": merge, "runLog": run_log("EVIDENCE-B"),
            }),
        )
        .await
        .unwrap();
    let settled = ctx.hub.test_get_gate_job(&job).await.unwrap().unwrap();
    assert_eq!(settled["state"], json!("landed"), "{settled}");
    assert_eq!(settled["logObjectId"], json!(a_id), "{settled}");
}

/// r20 item 8; r21 item 6: the ATTEMPT-CAP base-moved Failed settlement binds
/// ITS OWN result's evidence — TWO REAL result handlers with INDEPENDENT
/// post-persistence release gates. Both same-grant attempt-3 base-moved
/// results (A and B, different logs) enter through the real apply path and
/// park after persisting their own immutable objects. A is released ALONE and
/// must commit the cap Failed bound to A's object; the expected winner is A by
/// test construction (read from A's persisted object), NEVER inferred from the
/// row under test. Only then is B released, and the reference must remain A's.
/// Both objects' bytes are checked independently, before and after B.
#[tokio::test]
async fn an_attempt_cap_base_moved_failure_binds_its_own_results_evidence() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job = queued_land_job(&ctx).await;
    let merge = "7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c";
    let moved_base = "0123456789abcdef0123456789abcdef01234567";

    let run_log = |headline: &'static str| {
        json!({
            "step": "*", "kind": "failed", "headline": headline,
            "summary": [], "tail": [], "capturedLines": 1,
            "truncated": false, "attempts": 3,
        })
    };

    // Attempt 1: claim, then a real base-moved requeue through the running
    // path; the scheduler claims attempt 2.
    ctx.hub
        .test_claim_gate_job(&job, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed attempt 1");
    ctx.hub
        .test_apply_gate_result(
            &job,
            json!({
                "jobId": job, "generation": 1, "status": "base-moved",
                "mergeSha": merge, "currentMainSha": moved_base,
            }),
        )
        .await
        .unwrap();
    ctx.hub
        .test_claim_gate_job(&job, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("scheduler claims attempt 2");
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    // Attempt 2 is granted (Landing); its base-moved verdict requeues.
    ctx.hub
        .test_gate_land_grant(&job, &ctx.host, merge, session)
        .await
        .unwrap();
    ctx.hub
        .test_apply_gate_result(
            &job,
            json!({
                "jobId": job, "generation": 2, "status": "base-moved",
                "mergeSha": merge, "currentMainSha": moved_base,
            }),
        )
        .await
        .unwrap();
    // Attempt 3: claimed and granted — the row is Landing at the cap.
    ctx.hub
        .test_claim_gate_job(&job, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("scheduler claims attempt 3");
    ctx.hub
        .test_gate_land_grant(&job, &ctx.host, merge, session)
        .await
        .unwrap();
    let pre = ctx.hub.test_get_gate_job(&job).await.unwrap().unwrap();
    assert_eq!(pre["attempts"], json!(3), "{pre}");
    assert_eq!(pre["state"], json!("landing"), "{pre}");

    // Start BOTH REAL handlers; each parks AFTER persisting its own evidence
    // and before the settlement writer, under a key = its own object id.
    let barrier = ctx.store().gate_after_persist_barrier();
    barrier.arm();
    let spawn_handler = |headline: &'static str| {
        let state = ctx.hub.test_state().clone();
        let job = job.clone();
        tokio::spawn(async move {
            remuda_hub::RunningHub::test_apply_gate_result_static(
                &state,
                &job,
                json!({
                    "jobId": job, "generation": 3, "status": "base-moved",
                    "mergeSha": merge, "currentMainSha": moved_base,
                    "runLog": run_log(headline),
                }),
            )
            .await
        })
    };
    let apply_a = spawn_handler("EVIDENCE-A");
    let apply_b = spawn_handler("EVIDENCE-B");
    barrier.wait_parked(2).await;

    // Resolve each handler's OWN persisted object from the immutable table
    // (keyed by its distinct headline) BEFORE any settlement. A is the
    // expected winner by construction — not by reading the row under test.
    let objects: std::collections::HashMap<String, String> = {
        let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
        db.prepare("SELECT id, bytes FROM gate_log_objects WHERE job_id = ?1")
            .unwrap()
            .query_map(rusqlite::params![job], |row| {
                let id: String = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                Ok((id, bytes))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .map(|(id, bytes)| {
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                (value["headline"].as_str().unwrap().to_owned(), id)
            })
            .collect()
    };
    assert_eq!(
        objects.len(),
        2,
        "both real handlers persisted their own object"
    );
    let a_id = objects
        .get("EVIDENCE-A")
        .expect("A persisted its object")
        .clone();
    let b_id = objects
        .get("EVIDENCE-B")
        .expect("B persisted its object")
        .clone();
    assert_ne!(a_id, b_id, "A and B own distinct content-addressed objects");

    // Release A ALONE; B stays parked and cannot settle or rebind anything.
    barrier.release_key(&a_id).await;
    apply_a.await.unwrap();
    let doc = ctx.hub.test_get_gate_job(&job).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("failed"),
        "A commits the cap Failed: {doc}"
    );
    assert_eq!(
        doc["logObjectId"],
        json!(a_id),
        "A settles bound to A's OWN object while B is still parked: {doc}"
    );
    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{by_job}");
    assert_eq!(
        by_job["log"]["headline"],
        json!("EVIDENCE-A"),
        "by-job serves A's evidence before B is released: {by_job}"
    );

    // Now release B: it reaches the terminal guard and must stand down; the
    // reference stays A's.
    barrier.release_key(&b_id).await;
    apply_b.await.unwrap();
    let doc = ctx.hub.test_get_gate_job(&job).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("failed"), "{doc}");
    assert_eq!(
        doc["logObjectId"],
        json!(a_id),
        "B must not replace A's binding: {doc}"
    );

    // Each object serves exactly its OWN bytes, by object id.
    for (id, headline) in [(&a_id, "EVIDENCE-A"), (&b_id, "EVIDENCE-B")] {
        let (status, served) = ctx
            .send(
                "GET",
                &format!("/v1/gate/logs/{id}"),
                &ctx.human,
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{served}");
        assert_eq!(served["objectId"], json!(id), "object identity: {served}");
        assert_eq!(
            served["log"]["headline"],
            json!(headline),
            "{id} must serve its own persisted bytes: {served}"
        );
    }

    // By job id: the final reference serves ONLY A's evidence.
    let (status, by_job) = ctx
        .send(
            "GET",
            &format!("/v1/gate/logs/{job}"),
            &ctx.human,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{by_job}");
    assert_eq!(by_job["objectId"], json!(a_id), "{by_job}");
    assert_eq!(
        by_job["log"]["headline"],
        json!("EVIDENCE-A"),
        "the by-job reference stays bound to A's own evidence after B: {by_job}"
    );
}

/// r16 item 6: when a NEW carrier B takes over while the older socket A is
/// still live, B is a live TAKEOVER and must NOT inherit A's in-flight Landing
/// grant. The post-hello reconciliation runs on B (its gate.resolveLanding
/// probe completes), but the row's bound dispatch_session stays A — even after
/// the stale A socket tears down. A genuine reconnect (empty slot) would
/// rebind; the takeover must not.
#[tokio::test]
async fn a_live_takeover_socket_does_not_inherit_the_old_landing_grant() {
    let (ctx, node_a) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session_a = ctx.hub.test_node_session(&ctx.host).await.expect("A live");
    let merge = "ababababababababababababababababababababab";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session_a)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    assert_eq!(
        ctx.hub.test_gate_dispatch_session(&job_id).await.unwrap(),
        Some(session_a as i64)
    );

    // B connects while A is STILL OPEN (a socket-level takeover).
    let mut node_b = FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace)
        .await
        .unwrap();
    let session_b = ctx
        .hub
        .test_node_session(&ctx.host)
        .await
        .expect("B became current");
    assert_ne!(session_a, session_b);

    // Latch B's post-hello task to completion via its forced Landing probe.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout(Duration::from_millis(200), node_b.next_frame()).await {
            Ok((method, _)) if method == "gate.resolveLanding" => break,
            Ok(_) => {}
            Err(_) => assert!(
                std::time::Instant::now() < deadline,
                "B's post-hello Landing probe never ran"
            ),
        }
    }

    // B's post-hello reconciliation completed WITHOUT inheriting A's grant:
    // the Landing is still bound to session A.
    assert_eq!(
        ctx.hub.test_gate_dispatch_session(&job_id).await.unwrap(),
        Some(session_a as i64),
        "a live takeover must not rebind the old Landing grant to B"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");

    // The stale A socket now tears down AFTER B's hello. Nothing rebinds.
    node_a.disconnect();
    assert_eq!(
        ctx.hub.test_gate_dispatch_session(&job_id).await.unwrap(),
        Some(session_a as i64),
        "the stale A teardown must not transfer the Landing grant to B"
    );
    assert_eq!(
        ctx.hub.test_node_session(&ctx.host).await,
        Some(session_b),
        "B remains the live carrier"
    );
}

/// Shared setup for the r15 item 4 suite: a Landing parked outcome-unknown with
/// a known granted (attempt, epoch, merge) tuple on the live fake carrier.
async fn unknown_landing_fixture(ctx: &Ctx, node: &FakeNode) -> (String, String, u64, String) {
    let job_id = queued_land_job(ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx
        .hub
        .test_node_session(&ctx.host)
        .await
        .expect("carrier live");
    let merge = "cccccccccccccccccccccccccccccccccccccccc";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");
    let unknown = ctx
        .hub
        .test_mark_landing_unknown(&job_id, "the granted push was not confirmed ...")
        .await
        .unwrap();
    assert_eq!(unknown["state"], json!("landing"));
    // Keep the fake carrier connected (it owns `node`); ensure no stale probe
    // answer is queued.
    node.set_resolve_reply(json!({
        "jobId": job_id, "status": "unknown",
        "baseSha": null, "error": "test: no remote evidence"
    }))
    .await;
    let epoch = unknown["landingSessionEpoch"]
        .as_str()
        .expect("grant epoch stamped")
        .to_owned();
    (job_id, merge.to_owned(), session, epoch)
}

/// r15 item 4: an AGENT caller (even one holding the land grant) cannot resolve
/// an unknown Landing — the endpoint is operator-only and refuses 403.
#[tokio::test]
async fn an_agent_caller_cannot_resolve_an_unknown_landing() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let (job_id, merge, _session, epoch) = unknown_landing_fixture(&ctx, &node).await;
    let body = json!({
        "resolution": "failed",
        "expectedAttempts": 1,
        "expectedSessionEpoch": epoch,
        "expectedMergeSha": merge,
        "rationale": "agent must not be able to discard a landing",
    });
    let (status, reply) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project),
            body,
        )
        .await;
    assert_eq!(status, 403, "an agent caller must be refused: {reply}");
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "the row is untouched: {doc}"
    );
    assert_eq!(doc["landingOutcomeUnknown"], json!(true), "{doc}");
}

/// r15 item 4: the operator resolution must name the exact live granted tuple;
/// a paused frame against a newer/other attempt, a missing epoch/merge, or an
/// empty rationale is refused without touching the row. The exact tuple
/// settles.
#[tokio::test]
async fn an_operator_resolution_validates_its_expected_tuple_and_rationale() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let (job_id, merge, _session, good_epoch) = unknown_landing_fixture(&ctx, &node).await;
    let path = format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project);

    // Empty rationale -> 400.
    let (status, reply) = ctx
        .send(
            "POST",
            &path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": good_epoch,
                "expectedMergeSha": merge,
                "rationale": "   ",
            })),
        )
        .await;
    assert_eq!(status, 400, "{reply}");

    // Tuple naming a NEWER attempt (a paused frame after a re-dispatch) -> 409.
    let (status, reply) = ctx
        .send(
            "POST",
            &path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 7,
                "expectedSessionEpoch": good_epoch,
                "expectedMergeSha": merge,
                "rationale": "paused stale resolution",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "a newer/other attempt must be refused: {reply}"
    );

    // Wrong epoch and wrong merge -> 409.
    for body in [
        json!({
            "resolution": "failed", "expectedAttempts": 1,
            "expectedSessionEpoch": "epoch:totally-different",
            "expectedMergeSha": merge, "rationale": "wrong epoch",
        }),
        json!({
            "resolution": "failed", "expectedAttempts": 1,
            "expectedSessionEpoch": good_epoch,
            "expectedMergeSha": "dddddddddddddddddddddddddddddddddddddddd",
            "rationale": "wrong merge",
        }),
    ] {
        let (status, reply) = ctx.send("POST", &path, &ctx.human, None, Some(body)).await;
        assert_eq!(status, 409, "{reply}");
    }
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "refused resolutions never settle: {doc}"
    );

    // The exact live tuple, by the operator, with a rationale settles Failed.
    let (status, reply) = ctx
        .send(
            "POST",
            &path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": good_epoch,
                "expectedMergeSha": merge,
                "rationale": "operator verified the push is absent on origin out of band",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["state"], json!("failed"), "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or("")
            .contains("absent on origin out of band"),
        "the rationale is persisted on the row: {reply}"
    );
}

/// r20 item 6: a stale operator resolution naming attempt N while the job is on
/// an outcome-unknown attempt N+1 must be refused 409 WITHOUT sending
/// gate.abortLanding for the current (newer) attempt. The validation of the
/// full (attempt, epoch, merge) tuple happens before any abort side effect.
#[tokio::test]
async fn a_stale_operator_resolution_is_refused_409_without_aborting_the_newer_attempt() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let (job_id, merge_1, session, epoch_1) = unknown_landing_fixture(&ctx, &node).await;
    let path = format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project);

    // The operator genuinely requeues attempt 1: the abort for generation 1 is
    // sent and the row re-enters the queue.
    let (status, reply) = ctx
        .send(
            "POST",
            &path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "requeue",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch_1,
                "expectedMergeSha": merge_1,
                "rationale": "operator requeues attempt 1 for real",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    // Drain the frames the valid requeue sent (it probes first, then aborts):
    // the abort must name generation 1.
    let mut drained = 0;
    let mut saw_generation_one_abort = false;
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(200), node.next_frame()).await {
            Ok((method, params)) => {
                drained += 1;
                if method == "gate.abortLanding" {
                    assert_eq!(
                        params["generation"],
                        json!(1),
                        "the valid abort named attempt 1"
                    );
                    saw_generation_one_abort = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_generation_one_abort,
        "the valid requeue sent its generation-1 abort ({drained} frames)"
    );

    // A real attempt 2 is claimed, granted (different merge/epoch) and parks
    // outcome-unknown — the job is NOW on the newer attempt.
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("scheduler claims attempt 2");
    let merge_2 = "dddddddddddddddddddddddddddddddddddddddd";
    ctx.hub
        .test_gate_land_grant(&job_id, &ctx.host, merge_2, session)
        .await
        .unwrap();
    ctx.hub
        .test_mark_landing_unknown(&job_id, "attempt 2 also unconfirmed ...")
        .await
        .unwrap();
    let current = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(current["attempts"], json!(2), "{current}");
    // The test grant seam stamps a constant epoch for both attempts, but the
    // tuple diverges on attempts AND merge, which is enough to bind the abort
    // to the validated generation.

    // The DELAYED stale frame from attempt 1 arrives: it names the OLD tuple
    // (older attempt and older merge).
    let (status, reply) = ctx
        .send(
            "POST",
            &path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch_1,
                "expectedMergeSha": merge_1,
                "rationale": "stale paused frame for attempt 1",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "the stale tuple is refused before any side effect: {reply}"
    );

    // After the 409: the only node RPC the stale call makes is the read-only
    // resolveLanding probe. A gate.abortLanding for generation 2 must never
    // appear. Drain frames well past the response (any abort would have been
    // queued BEFORE the HTTP response returned).
    let mut saw_resolve_probe = false;
    let drain_deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(100), node.next_frame()).await {
            Ok((method, params)) => {
                assert_ne!(
                    method, "gate.abortLanding",
                    "the stale resolution aborted the newer attempt: {params}"
                );
                if method == "gate.resolveLanding" {
                    saw_resolve_probe = true;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_resolve_probe,
        "the stale call still made its read-only probe"
    );

    // The current Landing is untouched: attempt 2, still outcome-unknown.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert_eq!(doc["attempts"], json!(2), "{doc}");
    assert_eq!(doc["landingOutcomeUnknown"], json!(true), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge_2), "{doc}");
}

/// r15 item 4: a final read-only probe that ESTABLISHES the exact granted merge
/// on the remote wins over the operator's "failed" resolution: the response is
/// `landed`, never failed, and no operator failure is recorded.
#[tokio::test]
async fn an_established_landed_probe_wins_over_an_operator_failed_resolution() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let (job_id, merge, _session, epoch) = unknown_landing_fixture(&ctx, &node).await;
    // The lane's read-only probe now answers LANDED with the exact merge.
    node.set_resolve_reply(json!({
        "jobId": job_id, "status": "landed",
        "baseSha": "0123456789abcdef0123456789abcdef01234567",
    }))
    .await;
    let path = format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project);
    let (status, reply) = ctx
        .send(
            "POST",
            &path,
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator believed it failed",
            })),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        reply["state"],
        json!("landed"),
        "established remote landed must win over the operator failed: {reply}"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landed"), "{doc}");
    assert!(
        !doc["error"].as_str().unwrap_or("").contains("operator"),
        "no operator-failed error is recorded when the probe established landed: {doc}"
    );
}

/// r16 item 7: when the probe OBSERVES the exact merge landed on the remote but
/// the snapshot-settling writer refuses (the live row's bound grant session no
/// longer matches the responder), the operator resolution must not fall through
/// to failed/requeue and discard the fact. The request returns 409 and the row
/// stays Landing holding the landed merge.
#[tokio::test]
async fn an_observed_landed_fact_whose_settlement_is_refused_returns_conflict_not_failed() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let (job_id, merge, session, epoch) = unknown_landing_fixture(&ctx, &node).await;
    // Rebind the persisted grant to a DIFFERENT same-boot session: the live
    // carrier (answering the probe) is still the fixture session, so the
    // snapshot settle writer refuses while the probe establishes landed.
    ctx.hub
        .test_set_gate_dispatch_session_raw(&job_id, session.wrapping_add(1))
        .await
        .unwrap();
    node.set_resolve_reply(json!({
        "jobId": job_id, "status": "landed",
        "baseSha": "0123456789abcdef0123456789abcdef01234567",
    }))
    .await;
    let (status, reply) = ctx
        .send(
            "POST",
            &format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": 1,
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator believed it failed while the remote proved landed",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "an observed landed fact the writer cannot settle is a conflict, never failed: {reply}"
    );
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("landing"),
        "the Landing is kept, not discarded to failed/requeue: {doc}"
    );
    assert_eq!(doc["mergeSha"], json!(merge), "{doc}");
    assert!(
        doc["landingOutcomeUnknown"] == json!(true),
        "the row stays parked outcome-unknown for a valid later transition: {doc}"
    );
}

/// r15 item 5: a Landing whose bounded recovery window expires WHILE the lane
/// carrier is disconnected is still marked outcome-unknown — the expiry
/// evaluation happens before the connectivity check. The row keeps Landing and
/// its full occupancy (host/lane assignment), the operator resolve path stays
/// available, and the unknown journal is emitted.
#[tokio::test]
async fn a_disconnected_carrier_still_marks_an_expired_landing_unknown() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let merge = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    // Age the Landing past the 12-minute default recovery window.
    ctx.hub
        .test_set_landing_at(&job_id, "2000-01-01T00:00:00Z")
        .await
        .unwrap();

    // Carrier drops; wait bounded until the Hub has no live session for the
    // host.
    node.disconnect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while ctx.hub.test_node_session(&ctx.host).await.is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "host never went offline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The reconciler still runs (the periodic tick does the same); the row
    // becomes unknown with NO carrier available.
    ctx.hub.test_reconcile().await;

    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("landing"), "{doc}");
    assert_eq!(
        doc["landingOutcomeUnknown"],
        json!(true),
        "an expired Landing goes unknown even with no carrier: {doc}"
    );
    // Occupancy is fully retained.
    assert_eq!(doc["hostId"].as_str(), Some(ctx.host.as_str()), "{doc}");
    assert_eq!(doc["laneId"], json!("lane1"), "{doc}");
    assert_eq!(doc["mergeSha"], json!(merge), "{doc}");
    let actions = ctx.hub.test_gate_audit_actions(&job_id).await.unwrap();
    assert!(
        actions.iter().any(|a| a == "gate.landing-unknown"),
        "the unknown journal must fire while disconnected: {actions:?}"
    );

    // The operator resolve path stays available with no carrier: the final
    // probe is impossible, so the explicit operator decision applies.
    let epoch = doc["landingSessionEpoch"].as_str().unwrap();
    let (status, resolved) = ctx
        .send(
            "POST",
            &format!("/v1/projects/{}/gate/jobs/{job_id}/resolve", ctx.project),
            &ctx.human,
            None,
            Some(json!({
                "resolution": "failed",
                "expectedAttempts": doc["attempts"].as_u64().unwrap_or(1),
                "expectedSessionEpoch": epoch,
                "expectedMergeSha": merge,
                "rationale": "operator confirmed absence after the carrier was lost",
            })),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["state"], json!("failed"), "{resolved}");
}
