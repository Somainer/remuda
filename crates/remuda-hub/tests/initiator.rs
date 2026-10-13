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
async fn genuine_requeue_to_attempt_two(ctx: &Ctx, job_id: &str) {
    ctx.hub
        .test_apply_gate_result(
            job_id,
            json!({
                "jobId": job_id,
                "generation": 1,
                "status": "base-moved",
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

    // First arrival: persists the log under the stable per-job object id and
    // commits Failed with logObjectId set.
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

/// A pre-upgrade job references a LEGACY random object id A. A later runLog
/// result must reuse that id (not derive a new deterministic B and delete
/// A), including when a competing terminal write (the duplicate's first
/// arrival / grace finisher) makes THIS writer stop at the guard after
/// persisting. The job's stored logObjectId resolves by object id and by
/// job id.
#[tokio::test]
async fn a_late_runlog_reuses_the_jobs_legacy_log_object_id_through_a_terminal_interleave() {
    use remuda_hub::store_test_support::TestGateFinishBeforeWrite;

    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (job_id, legacy_id, _old_bytes) = running_job_with_legacy_log(&ctx).await;

    // The second handler's perspective: its pre-read saw Running and the
    // first duplicate arrival's Failed write lands inside its writer after
    // the persist. The persist must replace bytes UNDER THE LEGACY ID.
    ctx.store().test_arm_finish_before_gate_mutate(
        job_id.clone(),
        TestGateFinishBeforeWrite::DuplicateFailed,
    );
    ctx.hub
        .test_apply_gate_result(&job_id, failed_result_with_log(&job_id, "the NEW evidence"))
        .await
        .unwrap();

    // The job still references the legacy id, which now holds the new bytes.
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(doc["state"], json!("failed"), "{doc}");
    assert_eq!(
        doc["logObjectId"],
        json!(legacy_id),
        "the legacy reference must be reused, not orphaned: {doc}"
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
        json!("the NEW evidence"),
        "the reused object should carry the new bytes: {by_object}"
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
}

/// A failed INSERT inside the log writer must roll the prior DELETE back:
/// the previously referenced object stays served. The select/delete/insert
/// are one immediate transaction.
#[tokio::test]
async fn a_failed_log_insert_keeps_the_previous_object_served() {
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (job_id, legacy_id, old_bytes) = running_job_with_legacy_log(&ctx).await;

    // Make the writer fail its insert AFTER the delete (one-shot).
    ctx.store().test_arm_gate_log_insert_fault();
    ctx.hub
        .test_apply_gate_result(&job_id, failed_result_with_log(&job_id, "never stored"))
        .await
        .unwrap();

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
            Some(initiator.clone()),
            Some(launch_device.clone()),
        )
        .await
        .unwrap();
    assert!(created);
    store
        .mark_forward_intent(held.command_id.clone())
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
        .mark_forward_intent(second.command_id.clone())
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

/// r16 item 5: a `base-moved` verdict on a Landing row is the CAS-LOSS
/// requeue, not a terminal merge outcome — it must requeue even though it
/// carries no merge sha (the granted merge provably did not land), clearing
/// the old granted merge for a fresh verify. The merge bind only gates
/// terminal landed/failed verdicts.
#[tokio::test]
async fn a_base_moved_landing_verdict_without_a_merge_sha_still_requeues() {
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

    // CAS loss, no mergeSha in the verdict (it names the new remote base, not
    // the old merge).
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "generation": 1,
                "status": "base-moved",
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
/// 2. verdict: a `landed` finished frame for a different sha does not settle
///    the Landing, while the exact granted merge settles it;
/// 3. evidence: a log carrying a mismatched merge (or none at all) is rejected
///    before persistence even from the granted host+session.
#[tokio::test]
async fn the_granted_merge_is_bound_in_grant_verdict_and_evidence_writers() {
    let (ctx, node) = Ctx::boot().await.unwrap();
    ctx.configure_gate_lane().await;
    let job_id = queued_land_job(&ctx).await;
    ctx.hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap()
        .expect("claimed");
    let session = ctx.hub.test_node_session(&ctx.host).await.unwrap();
    let merge = "dddddddddddddddddddddddddddddddddddddddd";
    let other = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let grant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(grant["granted"], json!(true), "{grant}");

    // 1. Re-grant for a DIFFERENT merge on the same (attempt, host, session):
    //    refused, and the granted merge recorded on the row is unchanged.
    let regrant = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, other, session)
        .await
        .unwrap();
    assert_eq!(regrant["granted"], json!(false), "{regrant}");
    let unchanged = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(unchanged["state"], json!("landing"), "{unchanged}");
    assert_eq!(unchanged["mergeSha"], json!(merge), "{unchanged}");
    // The same merge still gets the idempotent granted=true re-answer.
    let again = ctx
        .hub
        .test_gate_land_grant(&job_id, &ctx.host, merge, session)
        .await
        .unwrap();
    assert_eq!(again["granted"], json!(true), "{again}");

    // 2. A landed verdict for the OTHER merge must not settle this Landing.
    //    Drive it through the production apply barrier: parked → released →
    //    completed, so the stand-down is observed deterministically (no sleeps).
    let barrier = ctx.store().gate_apply_barrier();
    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0", "method": "gate.event",
        "params": {
            "jobId": job_id, "generation": 1, "kind": "finished",
            "result": {"jobId": job_id, "generation": 1, "status": "landed",
                       "mergeSha": other},
        },
    }));
    barrier.wait_parked(1).await;
    barrier.release();
    barrier.wait_completed(1).await;
    let still = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(still["state"], json!("landing"), "{still}");
    assert_eq!(still["mergeSha"], json!(merge), "{still}");

    // 3. Evidence for the wrong merge (right host+session) is rejected; so is
    //    evidence that carries no merge identity at all.
    for auth_merge in [Some(other), None] {
        let rejected = ctx
            .hub
            .test_insert_gate_log(
                &ctx.project,
                &job_id,
                br#"{"headline":"WRONG-MERGE"}"#.to_vec(),
                format!("digest-{auth_merge:?}"),
                "dev",
                1,
                Some(session),
                Some(&ctx.host),
                auth_merge,
            )
            .await
            .unwrap();
        assert_eq!(rejected, None, "evidence {auth_merge:?} must be rejected");
    }
    let committed = ctx
        .hub
        .test_insert_gate_log(
            &ctx.project,
            &job_id,
            br#"{"headline":"GRANTED-MERGE"}"#.to_vec(),
            "digest-granted-merge".into(),
            "dev",
            1,
            Some(session),
            Some(&ctx.host),
            Some(merge),
        )
        .await
        .unwrap()
        .expect("evidence naming the exact granted merge commits");
    let _ = committed;

    // The correct verdict settles landed and keeps the exact merge.
    barrier.arm();
    node.send_raw(json!({
        "jsonrpc": "2.0", "method": "gate.event",
        "params": {
            "jobId": job_id, "generation": 1, "kind": "finished",
            "result": {"jobId": job_id, "generation": 1, "status": "landed",
                       "mergeSha": merge},
        },
    }));
    barrier.wait_parked(1).await;
    barrier.release();
    barrier.wait_completed(1).await;
    let landed = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(landed["state"], json!("landed"), "{landed}");
    assert_eq!(landed["mergeSha"], json!(merge), "{landed}");
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
