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
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

type AppendFrame = (String, Value);

const BRIEF: &str = "Do the tiny task.\nReply DONE <sha> or BLOCKED <reason>.\n";

/// Fake Node: accepts every Hub RPC, records `(method, params)`, and lets the
/// test push journal events.
struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    frames: mpsc::UnboundedReceiver<AppendFrame>,
    appends: tokio::sync::mpsc::UnboundedSender<AppendFrame>,
    /// interactionId ids still pending at the fake Node (an
    /// `interaction.requested` journal event inserts; an answer would
    /// remove — the Hub never answers for the Node).
    pending: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
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
        let pending =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
        let pending_task = pending.clone();
        let answered_task = pending.clone();
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
                        // Drain the Hub's append ack so the select goes back to
                        // watching RPC requests; the pending set above is
                        // already populated.
                        let _ = node.next().await;
                    }
                    frame = node.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                                // journal.append is node→hub: never a Hub RPC
                                // request to record or answer.
                                if frame.get("method").and_then(Value::as_str)
                                    == Some("journal.append") { continue; }
                                let Some(method) = frame.get("method").and_then(Value::as_str) else {
                                    continue;
                                };
                                if method == "interaction.answer" {
                                    let id = frame["params"]["interactionId"]
                                        .as_str()
                                        .unwrap_or("")
                                        .to_owned();
                                    answered_task.lock().await.remove(&id);
                                }
                                let _ = frame_tx.send((method.to_owned(), frame["params"].clone()));
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
                                if frame.get("id").is_some()
                                    && node.send(Message::Text(json!({
                                        "jsonrpc":"2.0","id":frame["id"],"result":result
                                    }).to_string().into())).await.is_err()
                                { break; }
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

    /// Send a Human command sentinel, then drain frames until it arrives:
    /// every earlier frame is a known fan-out, and an
    /// `interaction.answer` among them fails the test. Watchdog timeout
    /// only — no no-frame window is part of the assertion.
    async fn drain_until_sentinel_and_assert_no_answer(&mut self, ctx: &Ctx) {
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
        loop {
            let (method, params) = self.next().await;
            if method == "instance.send" {
                return;
            }
            assert_ne!(
                method, "interaction.answer",
                "fenced answer reached the Node: {params}"
            );
        }
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

    // Deterministic wait: the fake Node knows the requested interaction is
    // pending locally once the append is acked. The Hub's journal
    // projection is driven from the same append, so the local set and the
    // HTTP list are consistent without any poll.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if node.is_pending(&interaction_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("interaction becomes pending at the Node");

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

    // The fake Node never saw an answer (sentinel drains the frames and
    // fails on an interaction.answer queueing ahead).
    node.drain_until_sentinel_and_assert_no_answer(&ctx).await;

    // The Hub never mirrored an answer: still pending in the list.
    let list: Value = ctx
        .http
        .get(format!("{}/v1/interactions", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let state = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["interactionId"] == json!(interaction_id))
        .and_then(|row| row["state"].as_str())
        .unwrap_or("missing");
    assert_eq!(state, "pending", "answer committed before the Node: {list}");
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
    node.drain_until_sentinel_and_assert_no_answer(&ctx).await;
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
    node.drain_until_sentinel_and_assert_no_answer(&ctx).await;

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

    // F before the request: admit_node_op (worktree.lease) refuses inside
    // the handler; lease_refusal must keep the 409 `fenced` shape instead
    // of remapping it to a directory-binding CONFLICT.
    ctx.fence().await;
    let (status, body) = ctx.post_pool_task("fence-before-lease").await;
    assert_fenced(status, &body);
    assert!(
        ctx.task_list().await.is_empty(),
        "task row survived a pre-lease fence: {body}"
    );
    assert!(
        ctx.lease_rows().is_empty(),
        "lease row written despite a fenced admission"
    );
    // No worktree.lease reached the Node: the next frame is the sentinel.
    node.drain_until_sentinel_and_assert_no_answer(&ctx).await;
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
    node.drain_until_sentinel_and_assert_no_answer(&ctx).await;
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
    node.drain_until_sentinel_and_assert_no_answer(&ctx).await;
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
    let (ctx, _node) = Ctx::boot().await.unwrap();
    let (initiator, device_id) = ctx.agent_authority();

    // Connected host: admitted → sent intent → Node reply → settled.
    let op_sent = format!("nop_{}", uuid::Uuid::now_v7());
    let reached = ctx
        .hub
        .test_call_node_op(
            &ctx.host,
            "worker.provision",
            op_sent.clone(),
            Some(initiator.clone()),
            Some(device_id.clone()),
        )
        .await
        .unwrap();
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
