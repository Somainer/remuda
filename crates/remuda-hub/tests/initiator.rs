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

const BRIEF: &str = "Do the tiny task.\nReply DONE <sha> or BLOCKED <reason>.\n";

/// Fake Node: accepts every Hub RPC, records `(method, params)`, and lets the
/// test push journal events.
struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    frames: mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
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
        let (append_tx, mut append_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((instance_id, event)) = append_rx.recv() => {
                        if node.send(Message::Text(json!({
                            "jsonrpc":"2.0","id":"append","method":"journal.append",
                            "params":{"instanceId":instance_id,"event":event}
                        }).to_string().into())).await.is_err() { break; }
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
        Ok(Self::connect_with_bearer(hub, &creds.host, workspace, &creds.token).await?.0)
    }

    async fn next(&mut self) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(5), self.frames.recv())
            .await
            .expect("frame in time")
            .expect("sender alive")
    }

    /// Assert no frame arrives within a short settle window.
    async fn assert_no_frame(&mut self, label: &str) {
        tokio::select! {
            frame = self.frames.recv() => panic!("{label}: unexpected Node frame {frame:?}"),
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
    }

    fn append(&self, instance_id: &str, event: Value) {
        self.appends
            .send((instance_id.to_owned(), event))
            .expect("append channel");
    }

    /// Drop the Node connection abruptly; the Hub marks its host offline.
    fn disconnect(&mut self) {
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
        let mut builder = self
            .http
            .request(method.parse().unwrap(), format!("{}{}", self.base(), path))
            .bearer_auth(token);
        if let Some(instance) = narrow {
            builder = builder.header("x-remuda-instance-id", instance);
        }
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.unwrap();
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

    // gate job insert.
    let (status, body) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/fenced/verify","mode":"verify"}),
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
    // no gate job queued.
    assert_eq!(ctx.command_rows().await.len(), commands_before);
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
    let jobs = gates["jobs"].as_array().cloned().unwrap_or_default();
    assert!(
        jobs.is_empty(),
        "a gate job was queued after fence: {gates}"
    );
}

// ── 2. Authenticated before F ─────────────────────────────────────────────

#[tokio::test]
async fn request_that_passes_authentication_before_fence_is_refused_at_commit() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
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
    node.assert_no_frame("authenticated-before-F send").await;
}

// ── 3. Device-specific: the MCP token row is the one re-checked ───────────

#[tokio::test]
async fn request_authenticated_with_mcp_token_is_refused_when_only_that_device_row_is_deleted() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

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
    node.assert_no_frame("deleted-MCP send").await;

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
    let (ctx, mut node) = Ctx::boot().await?;

    let command_id = remuda_protocol::CommandId::new()
        .as_id()
        .as_str()
        .to_owned();
    let mut body = Ctx::send_body("instance.send");
    body["commandId"] = json!(command_id);

    // Take the host offline: the first POST queues a HELD row (offline host,
    // no forward intent, no frame). Wait until the Hub has marked it offline.
    node.disconnect();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let host: Value = ctx
                .http
                .get(format!("{}/v1/hosts/{}", ctx.base(), ctx.host))
                .bearer_auth(&ctx.human)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if host["online"] == json!(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
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
    let mut node = FakeNode::reconnect(&ctx.hub, &ctx.creds, &ctx.workspace).await?;
    let (status, replay) = ctx
        .agent_post(&format!("/v1/instances/{}/commands", ctx.instance), body)
        .await;
    assert_fenced(status, &replay);
    node.assert_no_frame("same-id replay after fence").await;
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
    // Wait for the Hub to mirror it as pending.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
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
            let pending = list["items"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .any(|row| row["interactionId"] == json!(interaction_id))
                })
                .unwrap_or(false);
            if pending {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    // Drain the list/read fan-out frames produced while waiting for the
    // pending row to appear; only the answer frame matters below.
    while tokio::time::timeout(Duration::from_millis(150), node.frames.recv())
        .await
        .is_ok()
    {}

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
    node.assert_no_frame("fenced interaction.answer").await;

    // The Hub never mirrored an answer: still pending.
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

// ── 6. Gate claim after the job is enqueued ────────────────────────────────

#[tokio::test]
async fn gate_job_fenced_before_claim_is_canceled_fenced_and_never_dispatched() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();

    let (status, job) = ctx
        .agent_post(
            &format!("/v1/projects/{}/gate", ctx.project),
            json!({"branch":"wt/gate/claim","mode":"verify"}),
        )
        .await;
    assert_eq!(status, 200, "{job}");
    let job_id = job["id"].as_str().unwrap().to_owned();

    // Fence lands inside the claim writer job (the scheduler tick after F).
    ctx.store()
        .test_arm_fence_before_authority_check(ctx.instance.clone());
    let claimed = ctx
        .hub
        .test_claim_gate_job(&job_id, "lane1", &ctx.host)
        .await
        .unwrap();
    assert!(claimed.is_none(), "a fenced job must not be claimed");

    let doc = ctx
        .hub
        .test_get_gate_job(&job_id)
        .await
        .unwrap()
        .expect("job still exists");
    assert_eq!(doc["state"], json!("canceled"), "{doc}");
    assert_eq!(doc["reason"], json!("fenced"), "{doc}");
    node.assert_no_frame("fenced gate claim").await;
}

// ── 7. Human token narrowed to a fenced chapter ───────────────────────────

#[tokio::test]
async fn human_token_narrowed_to_a_fenced_chapter_is_refused() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
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
    node.assert_no_frame("narrowed-human send").await;
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

// ── 10. worker.provision admission: no Node effect after fence ────────────

#[tokio::test]
async fn dispatch_after_fence_refuses_before_worker_provision() {
    let (ctx, mut node) = Ctx::boot().await.unwrap();
    ctx.fence().await;

    // Agent dispatch is now an Agent-admitted route (D-057 §2 decision 2).
    let (status, body) = ctx
        .agent_post(
            "/v1/workers/dispatch",
            json!({"projectId":ctx.project,"brief":BRIEF,"harness":"claude"}),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], json!("fenced"), "dispatch body: {body}");
    node.assert_no_frame("fenced dispatch").await;

    // No roster row written.
    let workers: Value = ctx
        .http
        .get(format!("{}/v1/workers?project={}", ctx.base(), ctx.project))
        .bearer_auth(&ctx.agent)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(workers["items"].as_array().unwrap().is_empty(), "{workers}");
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
