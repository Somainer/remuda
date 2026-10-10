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
        self.arrived.notified().await;
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

/// Service one inbound Hub RPC frame: track answer state, record the frame
/// for assertions, honor a reply hold, then answer with a canned result.
async fn service_inbound<S>(
    node: &mut WebSocketStream<S>,
    frames: &mpsc::UnboundedSender<AppendFrame>,
    pending: &tokio::sync::Mutex<HashSet<String>>,
    holds: &std::sync::Mutex<HashMap<String, ReplyHold>>,
    frame: Value,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
        while !hold.released.load(Ordering::SeqCst) {
            hold.release.notified().await;
        }
    }
    if frame.get("id").is_none() {
        return;
    }
    let result = match method {
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
    let _ = node
        .send(Message::Text(
            json!({"jsonrpc":"2.0","id":frame["id"],"result":result})
                .to_string()
                .into(),
        ))
        .await;
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
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
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
                        if node.send(Message::Text(frame.to_string().into())).await.is_err() { break; }
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
                                        &mut node,
                                        &frame_tx,
                                        &answered_task,
                                        &holds_task,
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
                                    &mut node,
                                    &frame_tx,
                                    &answered_task,
                                    &holds_task,
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
    // The cancel commit: Running -> Canceling with the pin and stamp the
    // real cancel writer records (no gate.cancel frame is needed here).
    let db = rusqlite::Connection::open(ctx.db_path()).unwrap();
    db.execute(
        "UPDATE gate_jobs
            SET state = 'canceling',
                doc_json = json_set(
                    json_set(
                        json_set(doc_json, '$.state', 'canceling'),
                        '$.cancelRequestedAt', '2020-01-01T00:00:00.000Z'),
                    '$.laneId', 'lane1'),
                revision = revision + 1,
                updated_at = '2020-01-01T00:00:00.000Z'
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
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567"
            }),
        )
        .await
        .unwrap();

    // A genuinely later third delivery now pre-reads Canceled as well and
    // must be ignored without any seam.
    ctx.hub
        .test_apply_gate_result(
            &job_id,
            json!({
                "jobId": job_id,
                "status": "base-moved",
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
                "currentMainSha": "0123456789abcdef0123456789abcdef01234567"
            }),
        )
        .await
        .unwrap();

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
    ctx.hub.test_gate_tick().await;
    let doc = ctx.hub.test_get_gate_job(&job_id).await.unwrap().unwrap();
    assert_eq!(
        doc["state"],
        json!("canceled"),
        "tick revived the job: {doc}"
    );
    node.assert_next_frame_is_sentinel(&ctx).await;
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
