//! D-060 decision 1 (cb-rootturn): the Hub records the root turn an Agent
//! call was made in — on the initiator, the audit row, the command row and the
//! frame it forwards to the Node — and never lets it decide anything.
//!
//! Everything drives a real Hub over HTTP with Human, Bot and Agent (MCP)
//! tokens and one fake Node that answers every RPC and journals exactly what a
//! structured (claude-sdk) carrier journals for an input: the user message
//! naming the delivering command, the `turn_started` whose
//! `nativeClientMessageId` is that command id, and the `result` naming the
//! turn it completed.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A fake Node: every RPC is recorded and answered `accepted`; journal
/// appends go out on the same socket.
struct FakeNode {
    frames: mpsc::UnboundedReceiver<(String, Value)>,
    appends: mpsc::UnboundedSender<(String, Value)>,
    /// What this Node lists for `interaction.list` (its own live cards).
    cards: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    async fn connect(
        hub: &remuda_hub::RunningHub,
        host: &str,
        workspace: &str,
        max_instances: u32,
    ) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
        ws.send(Message::Text(
            json!({
                "jsonrpc":"2.0","id":"hello","method":"node.hello",
                // A fence-capable Node: the seats are continuity lineages
                // (D-057 §7.6).
                "params": { "hostId": host, "capabilities": {"lineageFence": true}, "host": {
                    "hostname":"rootturn-node",
                    "workspaces":[{"workspaceId": workspace, "hostId": host, "root": "/srv/ws"}],
                    "workspaceRevision":1,
                    "maxInstances": max_instances,
                    "herdr":{"path":"/usr/bin/herdr"},
                    "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged-in"}]
                }}
            })
            .to_string()
            .into(),
        ))
        .await?;
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let hello: Value = serde_json::from_str(&text)?;
                anyhow::ensure!(hello.get("result").is_some(), "hello failed: {hello}");
            }
            other => anyhow::bail!("hello: {other:?}"),
        }
        let (frame_tx, frames) = mpsc::unbounded_channel();
        let (appends, mut append_rx) = mpsc::unbounded_channel::<(String, Value)>();
        let cards = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listed = cards.clone();
        let task = tokio::spawn(async move {
            let mut append_seq = 0u64;
            loop {
                tokio::select! {
                    Some((id, event)) = append_rx.recv() => {
                        append_seq += 1;
                        let frame = json!({
                            "jsonrpc":"2.0","id":format!("append-{append_seq}"),
                            "method":"journal.append",
                            "params":{"instanceId":id,"event":event}
                        });
                        if ws.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                            let Some(method) = frame["method"].as_str() else { continue };
                            let _ = frame_tx.send((method.to_owned(), frame["params"].clone()));
                            // `worker.provision` is the one RPC whose shape the
                            // Hub reads back (worktreePath/targetDir); every
                            // other call only needs acceptance.
                            let result = match method {
                                "worker.provision" => {
                                    let name = frame["params"]["name"].as_str().unwrap_or("w");
                                    json!({
                                        "name": name,
                                        "branch": frame["params"]["branch"].as_str().unwrap_or("wt/w/b"),
                                        "startPoint": "origin/main",
                                        "worktreePath": format!("/srv/remuda-wt/{name}"),
                                        "targetDir": format!("/srv/remuda-target/{name}")
                                    })
                                }
                                "interaction.list" => {
                                    json!({"items": listed.lock().expect("cards").clone()})
                                }
                                _ => json!({"accepted": true}),
                            };
                            let reply = json!({
                                "jsonrpc":"2.0","id":frame["id"],"result":result
                            });
                            if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        _ => {}
                    }
                }
            }
        });
        Ok(Self {
            frames,
            appends,
            cards,
            task,
        })
    }

    /// The next RPC the Hub sent whose method is `method`.
    async fn next(&mut self, method: &str) -> Value {
        loop {
            let (got, params) = tokio::time::timeout(TIMEOUT, self.frames.recv())
                .await
                .unwrap_or_else(|_| panic!("no {method} frame within the bounded wait"))
                .expect("frame channel closed");
            if got == method {
                return params;
            }
        }
    }
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Ctx {
    dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
}

impl Ctx {
    async fn boot() -> Result<(Self, FakeNode)> {
        Self::boot_with_capacity(8).await
    }

    /// [`Self::boot`] on a fake host that holds `max_instances` instances.
    async fn boot_with_capacity(max_instances: u32) -> Result<(Self, FakeNode)> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let human = hub.mint_device_token("rootturn-phone").await?;
        let host = HostId::new().as_id().as_str().to_owned();
        let workspace = remuda_protocol::WorkspaceId::new()
            .as_id()
            .as_str()
            .to_owned();
        let node = FakeNode::connect(&hub, &host, &workspace, max_instances).await?;
        Ok((
            Self {
                dir,
                hub,
                http: reqwest::Client::new(),
                human,
                host,
                workspace,
            },
            node,
        ))
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.hub.addr)
    }

    async fn post(
        &self,
        token: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Value,
    ) -> (u16, Value) {
        let mut request = self.http.post(self.url(path)).bearer_auth(token);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.json(&body).send().await.expect("http");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        self.http
            .get(self.url(path))
            .bearer_auth(&self.human)
            .send()
            .await
            .expect("http")
            .error_for_status()
            .expect("status")
            .json()
            .await
            .expect("json")
    }

    /// A claude-sdk seat with the dispatch grant, created by the owner, and
    /// its Agent (MCP) credential.
    async fn seat(&self, node: &mut FakeNode, name: &str) -> Result<(String, String)> {
        let (status, created) = self
            .post(
                &self.human,
                "/v1/instances",
                &[],
                json!({
                    "hostId": self.host, "kind": "claude", "driver": "claude-sdk",
                    "name": name, "permissionMode": "manual", "grants": ["dispatch"]
                }),
            )
            .await;
        anyhow::ensure!(status == 200, "seat create {status}: {created}");
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("instance id")?
            .to_owned();
        node.next("instance.create").await;
        let (status, token) = self
            .post(
                &self.human,
                &format!("/v1/instances/{id}/mcp-token"),
                &[],
                json!({}),
            )
            .await;
        anyhow::ensure!(status == 200, "mcp token {status}: {token}");
        Ok((id, token["token"].as_str().context("token")?.to_owned()))
    }

    async fn durable_seq(&self, instance: &str) -> u64 {
        self.get(&format!("/v1/instances/{instance}")).await["durableSeq"]
            .as_str()
            .and_then(|seq| seq.parse().ok())
            .unwrap_or(0)
    }

    /// Journal `event` on `instance` the way the Node does, and wait (bounded)
    /// until the Hub has folded it.
    async fn journal(&self, node: &FakeNode, instance: &str, event: Value) {
        let before = self.durable_seq(instance).await;
        node.appends
            .send((instance.to_owned(), event))
            .expect("append channel");
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while self.durable_seq(instance).await <= before {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the append was never folded"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Send `text` to `instance` with `token` and return the Hub command id.
    async fn send(&self, node: &mut FakeNode, token: &str, instance: &str, text: &str) -> String {
        let (status, body) = self
            .post(
                token,
                &format!("/v1/instances/{instance}/commands"),
                &[],
                json!({"operation":"instance.send","payload":{"input":{"type":"prompt","text":text}}}),
            )
            .await;
        assert_eq!(status, 200, "send: {body}");
        let params = node.next("instance.send").await;
        let id = body["command"]["commandId"]
            .as_str()
            .or_else(|| body["commandId"].as_str())
            .expect("command id")
            .to_owned();
        assert_eq!(
            params["commandId"],
            json!(id),
            "the Node got the Hub command id"
        );
        id
    }

    /// The structured carrier delivered `command` to `instance` and started
    /// its turn.
    async fn turn_starts(&self, node: &FakeNode, instance: &str, command: &str) {
        self.journal(node, instance, user_message(command)).await;
        self.journal(
            node,
            instance,
            turn_lifecycle("turn_started", json!({"nativeClientMessageId": command})),
        )
        .await;
    }

    /// An owner instruction that starts a turn on `seat`.
    async fn owner_turn(&self, node: &mut FakeNode, seat: &str) -> String {
        self.owner_turn_text(node, seat, "dispatch a worker").await
    }

    /// An owner instruction with `text` that starts a turn on `seat`.
    async fn owner_turn_text(&self, node: &mut FakeNode, seat: &str, text: &str) -> String {
        let human = self.human.clone();
        let command = self.send(node, &human, seat, text).await;
        self.turn_starts(node, seat, &command).await;
        command
    }

    /// [`Self::owner_turn_text`], journaling the user message with the
    /// owner's actual words, as a real carrier records the seat's input.
    async fn owner_turn_recorded(&self, node: &mut FakeNode, seat: &str, text: &str) -> String {
        let human = self.human.clone();
        let command = self.send(node, &human, seat, text).await;
        self.journal(node, seat, user_message_text(&command, text))
            .await;
        self.journal(
            node,
            seat,
            turn_lifecycle("turn_started", json!({"nativeClientMessageId": command})),
        )
        .await;
        command
    }

    /// `method path` as `token` with `headers`; the status and the RAW body,
    /// so a check sees exactly the bytes a caller receives.
    async fn call(
        &self,
        token: &str,
        headers: &[(String, String)],
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, String) {
        let mut request = self.http.request(method, self.url(path)).bearer_auth(token);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("http");
        let status = response.status().as_u16();
        (status, response.text().await.unwrap_or_default())
    }

    /// The owner's turn completes and the carrier runs one of its own with no
    /// new input: the seat now acts in an `agent` + `native` turn.
    async fn agent_continues(&self, node: &FakeNode, seat: &str, owner: &str) {
        self.journal(
            node,
            seat,
            turn_lifecycle(
                "result",
                json!({"completedTurn": owner, "queuedTurnCount": "1"}),
            ),
        )
        .await;
    }

    /// The seat is delivered a Hub-authored input (the shape a delivered fact
    /// or wake-up takes) and its turn starts: `hub` + `hub`.
    async fn hub_turn(&self, node: &FakeNode, seat: &str, text: &str) -> String {
        let (command, _) = self
            .hub
            .store()
            .expect("store")
            .queue_command(
                None,
                Some(seat.to_owned()),
                self.host.clone(),
                "instance.send".into(),
                json!({"instanceId": seat, "input": {"type": "prompt", "text": text}}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("hub command");
        self.turn_starts(node, seat, &command.command_id).await;
        command.command_id
    }

    /// A project whose only member is this host's workspace, as `worker
    /// dispatch` resolves one.
    async fn create_project(&self) -> String {
        self.create_project_with_capacity(8, "58600-58629").await
    }

    /// [`Self::create_project`] with room for `max_instances` workers and the
    /// port range they draw their blocks from (never bound by the fake). A
    /// larger project also lets one seat hold that many active children.
    async fn create_project_with_capacity(&self, max_instances: u32, ports: &str) -> String {
        let mut body = json!({
                    "name": "reqby-project",
                    "members": [{
                        "hostId": self.host, "workspaceId": self.workspace, "role": "build"
                    }],
                    "hosts": [{
                        "hostId": self.host, "maxInstances": max_instances, "maxBuilding": 4,
                        "diskBudgetGb": 10, "portBlocks": [ports],
                        "requires": [], "latencyClass": "remote"
                    }],
        });
        if max_instances > 8 {
            body["policy"] = json!({"configurable": {
                "coordinatorFanOut": max_instances,
                "maxConcurrentWorkers": max_instances,
            }});
        }
        let (status, body) = self.post(&self.human, "/v1/projects", &[], body).await;
        assert_eq!(status, 200, "project: {body}");
        body["id"].as_str().expect("project id").to_owned()
    }

    /// An Agent worker dispatch with one fixed brief, so a retry after
    /// approval is byte-identical.
    async fn worker_dispatch(
        &self,
        token: &str,
        project: &str,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
        self.worker_dispatch_body(token, project, headers, Value::Null)
            .await
    }

    /// [`Self::worker_dispatch`] with extra top-level body fields merged in.
    async fn worker_dispatch_body(
        &self,
        token: &str,
        project: &str,
        headers: &[(&str, &str)],
        extra: Value,
    ) -> (u16, Value) {
        let mut body = json!({
            "projectId": project,
            "brief": "the worker brief",
            "briefName": "brief.md",
            "harness": "claude",
        });
        merge_body(&mut body, &extra);
        self.post(token, "/v1/workers/dispatch", headers, body)
            .await
    }

    /// An Agent task creation with extra top-level body fields merged in.
    async fn create_task(&self, token: &str, project: &str, extra: Value) -> (u16, Value) {
        let mut body = json!({
            "projectId": project,
            "title": "the task",
            "intent": "ship it",
        });
        merge_body(&mut body, &extra);
        self.post(token, "/v1/tasks", &[], body).await
    }

    /// The pending one-shot approval card with `id`.
    async fn card(&self, id: &str) -> Value {
        let pending = self.get("/v1/interactions").await;
        pending["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["id"] == json!(id) || item["interactionId"] == json!(id))
            .cloned()
            .unwrap_or_else(|| panic!("card {id} in {pending}"))
    }

    /// Answer the card `id` allow-once, as the owner.
    async fn allow_once(&self, id: &str) -> u16 {
        let card = self.card(id).await;
        let digest = card
            .pointer("/request/inputDigest")
            .or_else(|| card.pointer("/interaction/request/inputDigest"))
            .and_then(Value::as_str)
            .expect("digest")
            .to_owned();
        let (status, body) = self
            .post(
                &self.human,
                &format!("/v1/interactions/{id}/answer"),
                &[],
                json!({"answer": {"kind": "approval", "optionId": "allow-once",
                                  "inputDigest": digest}}),
            )
            .await;
        assert_eq!(status, 200, "answer: {body}");
        status
    }

    /// The seat dispatches a child instance with its own credential; returns
    /// the child id and the params the Node got.
    async fn dispatch(&self, node: &mut FakeNode, token: &str) -> (String, Value) {
        let (status, body) = self.dispatch_request(token, &[]).await;
        assert_eq!(status, 200, "dispatch: {body}");
        let child = body["instance"]["instanceId"]
            .as_str()
            .expect("child id")
            .to_owned();
        let params = node.next("instance.create").await;
        (child, params)
    }

    async fn dispatch_request(&self, token: &str, headers: &[(&str, &str)]) -> (u16, Value) {
        self.post(
            token,
            "/v1/instances",
            headers,
            json!({
                "hostId": self.host, "kind": "claude", "driver": "claude-sdk",
                "permissionMode": "manual", "prompt": "the worker brief"
            }),
        )
        .await
    }

    async fn audit(&self, subject: &str) -> Vec<Value> {
        self.hub
            .store()
            .expect("store")
            .audit_for(subject.to_owned())
            .await
            .expect("audit")
    }

    /// The one `instance.dispatch` audit row for `child`.
    async fn dispatch_audit(&self, child: &str) -> Value {
        let rows = self.audit(child).await;
        let rows: Vec<&Value> = rows
            .iter()
            .filter(|row| row["action"] == "instance.dispatch")
            .collect();
        assert_eq!(rows.len(), 1, "one dispatch audit row: {rows:?}");
        rows[0].clone()
    }
}

fn user_message(command: &str) -> Value {
    user_message_text(command, "input")
}

fn user_message_text(command: &str, text: &str) -> Value {
    json!({"kind": "message", "payload": {
        "operation": "open", "messageId": format!("msg_{command}"), "role": "user",
        "phase": "input", "parentToolCallId": null, "commandId": command,
        "blocks": [{"type": "text", "text": text}]
    }})
}

fn turn_lifecycle(native_name: &str, related: Value) -> Value {
    json!({"kind": "lifecycle", "payload": {
        "type": "native", "topic": "turn", "nativeName": native_name,
        "nativeId": {"state": "known", "value": "session-1"},
        "status": {"state": "known", "value": if native_name == "turn_started" { "working" } else { "turn_done" }},
        "relatedIds": related, "dataRef": null, "severity": "info", "affectsCompletion": false
    }})
}

fn root_turn(command: &str, origin: &str, continuation: &str) -> Value {
    json!({"commandId": command, "origin": origin, "continuation": continuation})
}

/// Merge `extra`'s object fields into `body` (a no-op for a non-object).
fn merge_body(body: &mut Value, extra: &Value) {
    let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) else {
        return;
    };
    for (key, value) in extra {
        body.insert(key.clone(), value.clone());
    }
}

/// The same `requestedBy` record without the owner's words — what a non-Human
/// caller reads.
fn without_excerpt(record: &Value) -> Value {
    let mut record = record.clone();
    record.as_object_mut().expect("object").remove("excerpt");
    record
}

/// The first `requestedBy` anywhere in a task / board / worker payload.
fn first_requested_by(value: &Value) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if let Some(requested_by) = map.get("requestedBy") {
                return Some(requested_by.clone());
            }
            map.values().find_map(first_requested_by)
        }
        Value::Array(items) => items.iter().find_map(first_requested_by),
        _ => None,
    }
}

/// The pending card with `id` in an `/v1/interactions` page.
fn card_in(body: &Value, id: &str) -> Option<Value> {
    body["items"]
        .as_array()?
        .iter()
        .find(|item| item["id"] == json!(id) || item["interactionId"] == json!(id))
        .cloned()
}

/// An owner's instruction starts the seat's turn; the dispatch the seat makes
/// in it records that owner command as a human-origin root turn, on the
/// audit row, the child's create command row and the frame the Node journals.
#[tokio::test]
async fn an_owner_input_turn_records_origin_human_on_the_dispatch() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;

    // Before any input the Hub knows no turn: nothing is recorded, and the
    // dispatch still goes through.
    let (child, params) = ctx.dispatch(&mut node, &token).await;
    assert!(params["initiator"].is_object(), "{params}");
    assert!(params["initiator"].get("rootTurn").is_none(), "{params}");
    assert_eq!(
        ctx.dispatch_audit(&child).await["initiator"]["rootTurn"],
        Value::Null
    );

    let owner = ctx.owner_turn(&mut node, &seat).await;
    let (child, params) = ctx.dispatch(&mut node, &token).await;
    let expected = root_turn(&owner, "human", "none");

    let audit = ctx.dispatch_audit(&child).await;
    assert_eq!(audit["initiator"]["instanceId"], json!(seat));
    assert_eq!(audit["initiator"]["rootTurn"], expected, "{audit}");
    // The Node gets the same root turn on the initiator it persists and
    // journals with the command.
    assert_eq!(params["initiator"]["rootTurn"], expected, "{params}");
    // So does the Hub's own command row for the child's create.
    let commands = ctx.get(&format!("/v1/instances/{child}/commands")).await;
    let create = commands["commands"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["operation"] == "instance.create")
        })
        .cloned()
        .expect("create command row");
    assert_eq!(create["initiator"]["rootTurn"], expected, "{create}");
    // The request body never carries it: the create spec forwarded to the
    // Node has no rootTurn of its own.
    assert!(!params["spec"].to_string().contains("rootTurn"), "{params}");
    Ok(())
}

/// A turn the carrier starts with no new input continues the last input as
/// `agent` + `native`; an input the Hub authored is `hub` + `hub` (D-057
/// §6.4). Neither ever reads as human.
#[tokio::test]
async fn continuation_and_hub_turns_record_their_own_origins() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let owner = ctx.owner_turn(&mut node, &seat).await;

    // The owner's turn completes, but the carrier has a follow-up of its own
    // queued (a background task finished): no settle, no new input.
    ctx.journal(
        &node,
        &seat,
        turn_lifecycle(
            "result",
            json!({"completedTurn": owner, "queuedTurnCount": "1", "resultIndex": "0"}),
        ),
    )
    .await;
    let (child, _) = ctx.dispatch(&mut node, &token).await;
    assert_eq!(
        ctx.dispatch_audit(&child).await["initiator"]["rootTurn"],
        root_turn(&owner, "agent", "native")
    );

    // A Hub-authored input (no caller origin, no initiator), the shape a
    // delivered fact or wake-up takes, starts the next turn: it records the
    // real `hub` origin (D-057 §6.4, ma-restart).
    let (fact, _) = ctx
        .hub
        .store()
        .expect("store")
        .queue_command(
            None,
            Some(seat.clone()),
            ctx.host.clone(),
            "instance.send".into(),
            json!({"instanceId": seat, "input": {"type": "prompt", "text": "worker w1 is waiting"}}),
            None,
            None,
            None,
            None,
        )
        .await?;
    ctx.journal(
        &node,
        &seat,
        turn_lifecycle(
            "result",
            json!({"settledRootTurn": "true", "resultIndex": "1", "queuedTurnCount": "0"}),
        ),
    )
    .await;
    ctx.turn_starts(&node, &seat, &fact.command_id).await;
    let (child, params) = ctx.dispatch(&mut node, &token).await;
    let expected = root_turn(&fact.command_id, "hub", "hub");
    assert_eq!(
        ctx.dispatch_audit(&child).await["initiator"]["rootTurn"],
        expected
    );
    assert_eq!(params["initiator"]["rootTurn"], expected);
    Ok(())
}

/// Two seats in turns of different origins dispatch concurrently, many times
/// over: every row carries its own seat's root turn, never the other's.
#[tokio::test]
async fn concurrent_calls_never_share_or_swap_origins() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (owner_seat, owner_token) = ctx.seat(&mut node, "owner-seat").await?;
    let (bot_seat, bot_token) = ctx.seat(&mut node, "bot-seat").await?;
    let owner = ctx.owner_turn(&mut node, &owner_seat).await;
    let bot = ctx.hub.mint_bot_device_token("rootturn-bot").await?;
    let bot_command = ctx.send(&mut node, &bot, &bot_seat, "nightly check").await;
    ctx.turn_starts(&node, &bot_seat, &bot_command).await;

    // Two seats plus three dispatches each fills the fake host's eight slots.
    const ROUNDS: usize = 3;
    let calls = (0..ROUNDS).flat_map(|_| {
        [
            (owner_seat.clone(), owner_token.clone()),
            (bot_seat.clone(), bot_token.clone()),
        ]
    });
    let results = futures::future::join_all(calls.map(|(seat, token)| {
        let ctx = &ctx;
        async move {
            let (status, body) = ctx.dispatch_request(&token, &[]).await;
            assert_eq!(status, 200, "{body}");
            (
                seat,
                body["instance"]["instanceId"]
                    .as_str()
                    .expect("child")
                    .to_owned(),
            )
        }
    }))
    .await;
    for _ in 0..results.len() {
        node.next("instance.create").await;
    }
    for (seat, child) in results {
        let audit = ctx.dispatch_audit(&child).await;
        assert_eq!(audit["initiator"]["instanceId"], json!(seat), "{audit}");
        let expected = if seat == owner_seat {
            root_turn(&owner, "human", "none")
        } else {
            root_turn(&bot_command, "bot", "none")
        };
        assert_eq!(audit["initiator"]["rootTurn"], expected, "{audit}");
    }
    Ok(())
}

/// rootTurn is not a gate: the same calls get the same decisions whether the
/// caller is in an owner's turn, a Hub-authored one, or no known turn; and a
/// one-shot approval opened in an owner's turn is honoured from an
/// agent-origin turn, because only the caller and the exact request match.
#[tokio::test]
async fn an_authorization_decision_is_identical_with_and_without_root_turn() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let (_, ungranted) = {
        // A seat without the dispatch grant: its dispatch is refused.
        let (status, created) = ctx
            .post(
                &ctx.human,
                "/v1/instances",
                &[],
                json!({"hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
                       "name": "plain", "permissionMode": "manual"}),
            )
            .await;
        assert_eq!(status, 200, "{created}");
        let id = created["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_owned();
        node.next("instance.create").await;
        let (_, token) = ctx
            .post(
                &ctx.human,
                &format!("/v1/instances/{id}/mcp-token"),
                &[],
                json!({}),
            )
            .await;
        (id, token["token"].as_str().unwrap().to_owned())
    };

    // Decisions with no known turn.
    let refused_before = ctx.dispatch_request(&ungranted, &[]).await;
    let pending_before = ctx
        .dispatch_request(&token, &[("x-remuda-require-approval", "true")])
        .await;
    let (allowed_before, _) = ctx.dispatch_request(&token, &[]).await;
    node.next("instance.create").await;
    assert_eq!(refused_before.0, 403, "{:?}", refused_before.1);
    assert_eq!(pending_before.0, 409, "{:?}", pending_before.1);
    assert_eq!(allowed_before, 200);

    // The same calls inside an owner's turn.
    let owner = ctx.owner_turn(&mut node, &seat).await;
    let refused_owner = ctx.dispatch_request(&ungranted, &[]).await;
    let pending_owner = ctx
        .dispatch_request(&token, &[("x-remuda-require-approval", "true")])
        .await;
    let (allowed_owner, _) = ctx.dispatch_request(&token, &[]).await;
    node.next("instance.create").await;
    assert_eq!(refused_owner, refused_before, "a refusal does not move");
    assert_eq!(allowed_owner, allowed_before);
    assert_eq!(pending_owner.0, pending_before.0);
    assert_eq!(pending_owner.1["code"], pending_before.1["code"]);
    // The identical pending request reuses the one card it already opened:
    // the root turn is not part of what the grant matches.
    assert_eq!(
        pending_owner.1["interactionId"], pending_before.1["interactionId"],
        "a different turn did not open a second approval"
    );
    let approval = pending_owner.1["interactionId"]
        .as_str()
        .expect("approval id")
        .to_owned();

    // A Hub-authored turn takes over (agent origin), then the owner allows
    // the card. The replay from the agent-origin turn is honoured.
    let (fact, _) = ctx
        .hub
        .store()
        .expect("store")
        .queue_command(
            None,
            Some(seat.clone()),
            ctx.host.clone(),
            "instance.send".into(),
            json!({"instanceId": seat, "input": {"type": "prompt", "text": "a fact"}}),
            None,
            None,
            None,
            None,
        )
        .await?;
    ctx.journal(
        &node,
        &seat,
        turn_lifecycle(
            "result",
            json!({"completedTurn": owner, "settledRootTurn": "true"}),
        ),
    )
    .await;
    ctx.turn_starts(&node, &seat, &fact.command_id).await;
    let refused_fact = ctx.dispatch_request(&ungranted, &[]).await;
    assert_eq!(refused_fact, refused_before);

    let pending = ctx.get("/v1/interactions").await;
    let card = pending["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] == json!(approval) || item["interactionId"] == json!(approval))
        .cloned()
        .expect("the approval card");
    let digest = card
        .pointer("/request/inputDigest")
        .or_else(|| card.pointer("/interaction/request/inputDigest"))
        .and_then(Value::as_str)
        .expect("digest")
        .to_owned();
    let (status, body) = ctx
        .post(
            &ctx.human,
            &format!("/v1/interactions/{approval}/answer"),
            &[],
            json!({"answer": {"kind": "approval", "optionId": "allow-once", "inputDigest": digest}}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = ctx
        .dispatch_request(
            &token,
            &[
                ("x-remuda-require-approval", "true"),
                ("x-remuda-approval-id", approval.as_str()),
            ],
        )
        .await;
    assert_eq!(status, 200, "the allowed replay from another turn: {body}");
    let params = node.next("instance.create").await;
    assert_eq!(
        params["initiator"]["rootTurn"],
        root_turn(&fact.command_id, "hub", "hub"),
        "the replay itself records the turn it was made in"
    );
    Ok(())
}

/// `/approvals` reads the root turn off the card a request opened, so the
/// PWA can mark an agent-initiated request ("agent 自发").
#[tokio::test]
async fn an_approval_card_records_the_turn_that_asked() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let bot = ctx.hub.mint_bot_device_token("rootturn-bot").await?;
    let command = ctx.send(&mut node, &bot, &seat, "nightly check").await;
    ctx.turn_starts(&node, &seat, &command).await;
    let (status, body) = ctx
        .dispatch_request(&token, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{body}");
    let pending = ctx.get("/v1/interactions").await;
    let card = pending["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] == body["interactionId"])
        .cloned()
        .expect("card");
    assert_eq!(
        card["rootTurn"],
        root_turn(&command, "bot", "none"),
        "{card}"
    );
    Ok(())
}

/// A gate job an Agent enqueues records the root turn on its initiator, in
/// the job document the Hub persists and serves.
#[tokio::test]
async fn a_gate_job_records_the_root_turn_it_was_enqueued_in() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let (status, project) = ctx
        .post(
            &ctx.human,
            "/v1/projects",
            &[],
            json!({"name": "rootturn-project"}),
        )
        .await;
    assert_eq!(status, 200, "{project}");
    let project = project["id"].as_str().context("project id")?.to_owned();
    let owner = ctx.owner_turn(&mut node, &seat).await;
    let (status, job) = ctx
        .post(
            &token,
            &format!("/v1/projects/{project}/gate"),
            &[],
            json!({"branch": "wt/rootturn/verify", "mode": "verify"}),
        )
        .await;
    assert_eq!(status, 200, "{job}");
    let expected = root_turn(&owner, "human", "none");
    assert_eq!(job["initiator"]["rootTurn"], expected, "{job}");
    let jobs = ctx.get(&format!("/v1/projects/{project}/gate")).await;
    let stored = jobs["jobs"]
        .as_array()
        .or_else(|| jobs["items"].as_array())
        .and_then(|rows| rows.iter().find(|row| row["id"] == job["id"]))
        .cloned()
        .with_context(|| format!("job in listing: {jobs}"))?;
    assert_eq!(stored["initiator"]["rootTurn"], expected, "{stored}");
    Ok(())
}

// ── cb-requested-by: the owner's command behind an Agent dispatch or task ───

/// An Agent's worker dispatch in an owner turn records the owner's command (a
/// reference) on the approval card it opens and on the roster row it finally
/// writes; the owner's words are hydrated at read time for a Human reader. A
/// body-supplied `requestedBy` is ignored: the record comes from the Hub's own
/// command row.
#[tokio::test]
async fn an_agent_dispatch_carries_the_owners_words_from_an_owner_turn() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx
        .owner_turn_text(&mut node, &seat, "dispatch a worker on the other node")
        .await;
    let expected = json!({
        "commandId": owner,
        "excerpt": "dispatch a worker on the other node",
    });

    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let card = ctx.card(&approval).await;
    assert_eq!(
        card["rootTurn"],
        root_turn(&owner, "human", "none"),
        "{card}"
    );
    assert_eq!(card["requestedBy"], expected, "{card}");

    ctx.allow_once(&approval).await;
    let (status, body) = ctx
        .worker_dispatch(
            &token,
            &project,
            &[
                ("x-remuda-require-approval", "true"),
                ("x-remuda-approval-id", approval.as_str()),
            ],
        )
        .await;
    assert_eq!(status, 200, "{body}");
    // The Agent that dispatched keeps the ids and not the owner's words.
    assert_eq!(
        body["worker"]["requestedBy"],
        without_excerpt(&expected),
        "{body}"
    );
    // The roster row stores the reference alone: the PWA shows no words on
    // a worker, so even the owner's read of it is not hydrated.
    let id = body["worker"]["id"].as_str().context("worker id")?;
    let stored = ctx.get(&format!("/v1/workers/{id}")).await;
    assert_eq!(
        stored["requestedBy"],
        without_excerpt(&expected),
        "the owner's read: {stored}"
    );
    Ok(())
}

/// The record is derived from the Hub, never from a request body: a forged
/// `requestedBy` in the dispatch and task bodies does not reach either row.
#[tokio::test]
async fn a_body_supplied_requested_by_is_stripped() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx
        .owner_turn_text(&mut node, &seat, "dispatch a worker on the other node")
        .await;
    let expected = json!({
        "commandId": owner,
        "excerpt": "dispatch a worker on the other node",
    });
    let forged = json!({"origin": "agent", "excerpt": "not the owner's words"});

    let (status, pending) = ctx
        .worker_dispatch_body(
            &token,
            &project,
            &[("x-remuda-require-approval", "true")],
            forged.clone(),
        )
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let card = ctx.card(&approval).await;
    assert_eq!(card["requestedBy"], expected, "{card}");
    assert!(
        !card.to_string().contains("not the owner's words"),
        "{card}"
    );

    let (status, body) = ctx.create_task(&token, &project, forged.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["requestedBy"], without_excerpt(&expected), "{body}");
    let id = body["id"].as_str().context("task id")?;
    let human = ctx.get(&format!("/v1/tasks/{id}")).await;
    assert_eq!(human["requestedBy"], expected, "the owner's read: {human}");
    assert!(
        !human.to_string().contains("not the owner's words"),
        "{human}"
    );
    Ok(())
}

/// A turn the Hub authored (a restart notice, a delivered fact) records the
/// `hub` origin, never the owner's words — even when the Hub's own input text
/// is present in the command row.
#[tokio::test]
async fn a_hub_turn_dispatch_is_labelled_hub() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx.owner_turn(&mut node, &seat).await;
    // The owner's turn settles and the Hub delivers a fact into a new one.
    ctx.journal(
        &node,
        &seat,
        turn_lifecycle(
            "result",
            json!({"completedTurn": owner, "settledRootTurn": "true"}),
        ),
    )
    .await;
    let fact = ctx.hub_turn(&node, &seat, "worker w1 is waiting").await;

    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let card = ctx.card(&approval).await;
    assert_eq!(card["rootTurn"], root_turn(&fact, "hub", "hub"), "{card}");
    assert_eq!(card["requestedBy"], json!({"origin": "hub"}), "{card}");
    Ok(())
}

/// A turn the seat started on its own is labelled `agent`, with no owner words.
#[tokio::test]
async fn an_agent_turn_dispatch_is_labelled_agent() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx.owner_turn(&mut node, &seat).await;
    ctx.agent_continues(&node, &seat, &owner).await;

    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let card = ctx.card(&approval).await;
    assert_eq!(card["requestedBy"], json!({"origin": "agent"}), "{card}");
    assert!(
        !card["requestedBy"]
            .as_object()
            .unwrap()
            .contains_key("excerpt"),
        "{card}"
    );
    Ok(())
}

/// The excerpt is bounded at a char boundary and never carries a
/// credential-shaped string, even when the owner's input does.
#[tokio::test]
async fn the_excerpt_is_bounded_and_never_contains_a_secret_shape() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    // Assembled at run time so no source line reads as a token. The secret
    // sits INSIDE the bound, so the 280-char cap cannot be what removes it:
    // only the scrub can.
    let secret = ["sk", "-live-abcd1234efgh5678ijkl"].concat();
    let long = format!("他说：{} {}", secret, "继续".repeat(200));
    let owner = ctx.owner_turn_text(&mut node, &seat, &long).await;

    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let card = ctx.card(&approval).await;
    assert_eq!(card["requestedBy"]["commandId"], json!(owner), "{card}");
    let excerpt = card["requestedBy"]["excerpt"].as_str().context("excerpt")?;
    assert!(
        excerpt.chars().count() <= 280,
        "excerpt is {} chars: {excerpt}",
        excerpt.chars().count()
    );
    assert!(!excerpt.contains("abcd1234"), "{excerpt}");
    assert!(
        excerpt.contains("[redacted]"),
        "the scrub ran and withheld the token: {excerpt}"
    );
    assert!(!excerpt.contains('\n'), "one line: {excerpt}");
    Ok(())
}

/// The record is never a gate: an approval opened for a dispatch in one turn is
/// honoured from a later, different turn because the grant matches the caller
/// and the exact request — the record only changes what the card displays.
#[tokio::test]
async fn a_requested_by_never_changes_an_authorization_decision() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx
        .owner_turn_text(&mut node, &seat, "dispatch a worker on the other node")
        .await;
    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    assert_eq!(
        ctx.card(&approval).await["requestedBy"]["commandId"],
        json!(owner),
        "the owner turn's record is what the card shows"
    );
    ctx.allow_once(&approval).await;

    // The seat is now in a turn of its own; the same request is honoured.
    ctx.agent_continues(&node, &seat, &owner).await;
    let (status, body) = ctx
        .worker_dispatch(
            &token,
            &project,
            &[
                ("x-remuda-require-approval", "true"),
                ("x-remuda-approval-id", approval.as_str()),
            ],
        )
        .await;
    assert_eq!(status, 200, "the approved replay from another turn: {body}");
    // The roster row records the turn the dispatch actually ran in.
    assert_eq!(
        body["worker"]["requestedBy"],
        json!({"origin": "agent"}),
        "{body}"
    );
    Ok(())
}

/// An Agent-created task records a reference to the owner's command on its
/// `doc_json` (the words are hydrated for a Human read, never stored), and the
/// record survives a read and a split (the child records its own creation).
#[tokio::test]
async fn an_agent_created_task_records_the_owners_words() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx
        .owner_turn_text(&mut node, &seat, "split this into two boards")
        .await;
    let expected = json!({"commandId": owner, "excerpt": "split this into two boards"});

    let (status, task) = ctx.create_task(&token, &project, Value::Null).await;
    assert_eq!(status, 200, "{task}");
    assert_eq!(task["requestedBy"], without_excerpt(&expected), "{task}");
    let id = task["id"].as_str().context("task id")?;
    let stored = ctx.get(&format!("/v1/tasks/{id}")).await;
    assert_eq!(
        stored["requestedBy"], expected,
        "the owner's read: {stored}"
    );

    let (status, child) = ctx
        .post(
            &token,
            &format!("/v1/tasks/{id}/split"),
            &[],
            json!({"title": "child", "intent": "the second half"}),
        )
        .await;
    assert_eq!(status, 200, "{child}");
    assert_eq!(child["requestedBy"], without_excerpt(&expected), "{child}");
    let child_id = child["id"].as_str().context("child id")?;
    let child_stored = ctx.get(&format!("/v1/tasks/{child_id}")).await;
    assert_eq!(
        child_stored["requestedBy"], expected,
        "the owner's read: {child_stored}"
    );
    Ok(())
}

/// A task an Agent creates in a turn of its own records the agent origin.
#[tokio::test]
async fn an_agent_created_task_in_an_agent_turn_is_labelled_agent() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx.owner_turn(&mut node, &seat).await;
    ctx.agent_continues(&node, &seat, &owner).await;

    let (status, task) = ctx.create_task(&token, &project, Value::Null).await;
    assert_eq!(status, 200, "{task}");
    assert_eq!(task["requestedBy"], json!({"origin": "agent"}), "{task}");
    Ok(())
}

/// A create's opening prompt (`initialInput.text` / `spec.prompt`) is the
/// owner's input too: a dispatch in that first turn carries the excerpt.
#[tokio::test]
async fn an_owner_create_prompt_is_the_first_turns_excerpt() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (status, created) = ctx
        .post(
            &ctx.human,
            "/v1/instances",
            &[],
            json!({
                "hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
                "name": "seeded", "permissionMode": "manual", "grants": ["dispatch"],
                "prompt": "the owner opened this seat with these words",
            }),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let seat = created["instance"]["instanceId"]
        .as_str()
        .context("instance id")?
        .to_owned();
    let create_command = created["command"]["commandId"]
        .as_str()
        .context("create command id")?
        .to_owned();
    node.next("instance.create").await;
    let (status, token) = ctx
        .post(
            &ctx.human,
            &format!("/v1/instances/{seat}/mcp-token"),
            &[],
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{token}");
    let token = token["token"].as_str().context("token")?.to_owned();
    // The Node delivers the create's opening prompt as that command's input.
    ctx.turn_starts(&node, &seat, &create_command).await;

    let project = ctx.create_project().await;
    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let card = ctx.card(&approval).await;
    assert_eq!(
        card["rootTurn"]["commandId"],
        json!(create_command),
        "{card}"
    );
    assert_eq!(
        card["requestedBy"],
        json!({
            "commandId": create_command,
            "excerpt": "the owner opened this seat with these words",
        }),
        "{card}"
    );
    Ok(())
}

// ── cb-requested-by r3: the owner's words are default-deny ──────────────────

/// A string from the owner's input that nothing but the owner's own command
/// row and the seat's own input record may ever hold. ASCII, so a raw JSON
/// body carries it verbatim if a route leaks it.
const SENTINEL: &str = "reqby-sentinel-8c1f";

/// A string that only ever appears in a Node-supplied `requestedBy` (r4): the
/// Hub must neither store it nor serve it.
const NODE_SENTINEL: &str = "reqby-sentinel-node-4d2a";

/// Where [`scan_data_dir`] found the sentinel: the file, and for a SQLite
/// file the table, the column and the row's text and integer cells.
#[derive(Debug)]
struct Hit {
    file: String,
    table: Option<String>,
    column: Option<String>,
    row: serde_json::Map<String, Value>,
}

/// Every place under the Hub's data directory that holds `needle`: each cell
/// of each table of each SQLite database (text and blobs), and the raw bytes
/// of every other file. A database's `-wal`/`-shm` sidecars are skipped: their
/// pages are read through the database itself.
fn scan_data_dir(root: &std::path::Path, needle: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("data dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let name = path.to_string_lossy().into_owned();
            if name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.starts_with(b"SQLite format 3\0") {
                hits.extend(scan_sqlite(&path, needle));
            } else if contains_bytes(&bytes, needle.as_bytes()) {
                hits.push(Hit {
                    file: name,
                    table: None,
                    column: None,
                    row: serde_json::Map::new(),
                });
            }
        }
    }
    hits
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn scan_sqlite(path: &std::path::Path, needle: &str) -> Vec<Hit> {
    use rusqlite::{OpenFlags, types::ValueRef};
    let file = path.to_string_lossy().into_owned();
    let conn = rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open a Hub database read-only");
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("tables")
        .query_map([], |row| row.get(0))
        .expect("tables")
        .collect::<rusqlite::Result<_>>()
        .expect("tables");
    let mut hits = Vec::new();
    for table in tables {
        let sql = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
        let mut statement = conn.prepare(&sql).expect("select");
        let columns: Vec<String> = statement
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let mut rows = statement.query([]).expect("rows");
        while let Some(row) = rows.next().expect("row") {
            let mut cells = serde_json::Map::new();
            let mut matched = Vec::new();
            for (index, column) in columns.iter().enumerate() {
                let text = match row.get_ref(index).expect("cell") {
                    ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                        if contains_bytes(bytes, needle.as_bytes()) {
                            matched.push(column.clone());
                        }
                        String::from_utf8_lossy(bytes).into_owned()
                    }
                    ValueRef::Integer(value) => {
                        cells.insert(column.clone(), json!(value));
                        continue;
                    }
                    _ => continue,
                };
                cells.insert(column.clone(), Value::String(text));
            }
            for column in matched {
                hits.push(Hit {
                    file: file.clone(),
                    table: Some(table.clone()),
                    column: Some(column),
                    row: cells.clone(),
                });
            }
        }
    }
    hits
}

/// (a) Store-level default-deny: after an owner-turn dispatch, its approval,
/// a task create and a split, a string from the owner's input is stored only
/// in the owner's command row and in the seat's own input record — never in a
/// task doc, a roster row, an interaction, a journal payload or any other
/// table or file the Hub writes.
#[tokio::test]
async fn the_owners_words_are_stored_only_in_the_command_row() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let text = format!("{SENTINEL}: dispatch a worker on the other node");
    let owner = ctx.owner_turn_recorded(&mut node, &seat, &text).await;

    // An owner-turn dispatch that needs the D-017 approval, approved and run.
    let gated = [("x-remuda-require-approval", "true")];
    let (status, pending) = ctx.worker_dispatch(&token, &project, &gated).await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    ctx.allow_once(&approval).await;
    let (status, sent) = ctx
        .worker_dispatch(
            &token,
            &project,
            &[
                ("x-remuda-require-approval", "true"),
                ("x-remuda-approval-id", approval.as_str()),
            ],
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    // A second card left pending, and a task with a split child.
    let (status, pending) = ctx.worker_dispatch(&token, &project, &gated).await;
    assert_eq!(status, 409, "{pending}");
    let pending_card = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let (status, task) = ctx.create_task(&token, &project, Value::Null).await;
    assert_eq!(status, 200, "{task}");
    let task_id = task["id"].as_str().context("task id")?;
    let (status, child) = ctx
        .post(
            &token,
            &format!("/v1/tasks/{task_id}/split"),
            &[],
            json!({"title": "child", "intent": "the second half"}),
        )
        .await;
    assert_eq!(status, 200, "{child}");
    // Every record names the owner's command; the reads below are what the
    // scan must see stored.
    for (what, record) in [
        ("roster row", &sent["worker"]["requestedBy"]),
        ("task", &task["requestedBy"]),
        ("split child", &child["requestedBy"]),
    ] {
        assert_eq!(record, &json!({"commandId": owner}), "{what}");
    }

    let hits = scan_data_dir(&ctx.dir.path().join("data"), SENTINEL);
    // The seat's own input record: the user message naming that command.
    let input_row = |hit: &Hit| {
        let payload: Value = hit
            .row
            .get("payload_json")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        hit.table.as_deref() == Some("journal")
            && hit.row.get("instance_id") == Some(&json!(seat))
            && payload["kind"] == "message"
            && payload["payload"]["role"] == "user"
            && payload["payload"]["commandId"] == json!(owner)
    };
    let input_rows: Vec<(Option<&Value>, Option<&Value>)> = hits
        .iter()
        .filter(|hit| input_row(hit))
        .map(|hit| (hit.row.get("instance_id"), hit.row.get("seq")))
        .collect();
    // cb-search: the journal search index holds a redacted copy of that same
    // row, keyed by its (instance, seq) and deleted with it; the FTS table is
    // external content over the index rows and stores no text of its own.
    let index_row = |hit: &Hit| {
        hit.table.as_deref() == Some("journal_search_docs")
            && input_rows.contains(&(hit.row.get("instance_id"), hit.row.get("seq")))
    };
    let index_texts: Vec<Option<&Value>> = hits
        .iter()
        .filter(|hit| index_row(hit))
        .map(|hit| hit.row.get("text"))
        .collect();
    let allowed = |hit: &Hit| match hit.table.as_deref() {
        // The owner's own command row.
        Some("commands") => hit.row.get("id") == Some(&json!(owner)),
        Some("journal") => input_row(hit),
        Some("journal_search_docs") => index_row(hit),
        Some("journal_fts") => index_texts.contains(&hit.row.get("text")),
        _ => false,
    };
    let leaks: Vec<String> = hits
        .iter()
        .filter(|hit| !allowed(hit))
        .map(|hit| {
            format!(
                "{} {:?}.{:?}: {:?}",
                hit.file, hit.table, hit.column, hit.row
            )
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "the owner's words are stored outside the command row:\n{}",
        leaks.join("\n")
    );
    // The scan is not blind: it finds the two places the words belong.
    for table in ["commands", "journal"] {
        assert!(
            hits.iter().any(|hit| hit.table.as_deref() == Some(table)),
            "the scan saw no {table} row with the owner's words: {hits:?}"
        );
    }

    // A Node's interaction payload is untrusted: a record it supplies is
    // dropped at ingest, so the same scan finds no part of it — in the
    // interaction row, the journal row or anywhere else.
    ctx.journal(
        &node,
        &seat,
        json!({
            "kind": "interaction.requested",
            "payload": {
                "interactionKind": "approval",
                "interaction": {
                    "id": "int_node_supplied", "kind": "approval", "state": "pending",
                    "blocking": true, "answerable": true, "carrier": "harness-hook",
                    "deadline": {"state": "unknown"}, "resolution": {"state": "unknown"},
                    "request": {"kind": "approval", "title": "Bash",
                                "description": "a Node's own card", "options": []},
                    "requestedBy": {"commandId": "cmd_node", "excerpt": NODE_SENTINEL},
                }
            }
        }),
    )
    .await;
    let supplied = scan_data_dir(&ctx.dir.path().join("data"), NODE_SENTINEL);
    assert!(
        supplied.is_empty(),
        "a Node's requestedBy was stored: {supplied:?}"
    );

    // The approval cards live in the Hub's memory, not a table: a Bot reads
    // them exactly as stored (nothing hydrates a Bot), and they hold the
    // reference alone.
    let bot = ctx.hub.mint_bot_device_token("reqby-bot").await?;
    let (status, cards) = ctx
        .call(&bot, &[], reqwest::Method::GET, "/v1/interactions", None)
        .await;
    assert_eq!(status, 200, "{cards}");
    assert!(
        !cards.contains(SENTINEL),
        "a stored card holds the words: {cards}"
    );
    let cards: Value = serde_json::from_str(&cards)?;
    let card = card_in(&cards, &pending_card).context("the pending card")?;
    assert_eq!(card["requestedBy"], json!({"commandId": owner}), "{card}");
    Ok(())
}

/// Who calls in [`the_owners_words_reach_no_agent_or_bot_route`].
#[derive(Clone, Copy, PartialEq, Debug)]
enum Caller {
    /// An Agent of another lineage, with `dispatch`, in the same project.
    OtherLineage,
    /// A Bot, which relays to a chat platform.
    Bot,
    /// The seat the owner spoke to, with its own credential.
    Seat,
    /// The owner's own token narrowed to the seat (`x-remuda-instance-id`).
    OwnerAsSeat,
}

/// What a sweep call must return so the no-sentinel check is not vacuous.
enum Want {
    /// 200, and the body names the owner's command: the doc that carries the
    /// record was really served.
    Entity,
    /// Exactly this status.
    Status(u16),
    /// Any status: a route admitted for this caller that carries no record.
    Any,
}

/// (b) Route-level default-deny: every Agent- or Bot-admitted route that
/// returns a task, a roster row or an interaction — the agent_scope task,
/// worker and interaction allowlist entries, observe, placements, the worker
/// verbs and dispatch — never puts the owner's words in the raw body, for an
/// Agent of another lineage, a Bot, the seat itself and the owner narrowed to
/// the seat.
#[tokio::test]
async fn the_owners_words_reach_no_agent_or_bot_route() -> Result<()> {
    let (ctx, mut node) = Ctx::boot_with_capacity(64).await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let (_, other_lineage) = ctx.seat(&mut node, "other-lineage").await?;
    let bot = ctx.hub.mint_bot_device_token("reqby-bot").await?;
    let project = ctx.create_project_with_capacity(64, "58600-58959").await;
    let text = format!("{SENTINEL}: dispatch a worker on the other node");
    let owner = ctx.owner_turn_recorded(&mut node, &seat, &text).await;

    // In the owner's turn the seat opens a card (left pending, so the card
    // reads below serve a record) and, for each caller, creates a task and
    // dispatches a worker of its own.
    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let callers = [
        Caller::OtherLineage,
        Caller::Bot,
        Caller::Seat,
        Caller::OwnerAsSeat,
    ];
    let mut owned = Vec::new();
    for (index, caller) in callers.into_iter().enumerate() {
        let (status, task) = ctx.create_task(&token, &project, Value::Null).await;
        assert_eq!(status, 200, "{task}");
        // Named: the Hub's auto-name repeats within a project for weeks.
        let (status, sent) = ctx
            .worker_dispatch_body(
                &token,
                &project,
                &[],
                json!({"name": format!("reqby-w{index}")}),
            )
            .await;
        assert_eq!(status, 200, "{sent}");
        owned.push((
            caller,
            task["id"].as_str().context("task id")?.to_owned(),
            sent["worker"]["id"]
                .as_str()
                .context("worker id")?
                .to_owned(),
        ));
    }

    // (b) Every caller sweeps every route; collect every leak and every
    // unexpected status so one run reports them all.
    let mut problems = Vec::new();
    let seat_header = vec![("x-remuda-instance-id".to_string(), seat.clone())];
    for (index, (caller, task_id, worker_id)) in owned.iter().enumerate() {
        let (credential, headers) = match caller {
            Caller::OtherLineage => (other_lineage.as_str(), Vec::new()),
            Caller::Bot => (bot.as_str(), Vec::new()),
            Caller::Seat => (token.as_str(), Vec::new()),
            Caller::OwnerAsSeat => (ctx.human.as_str(), seat_header.clone()),
        };
        let agent = *caller != Caller::Bot;
        let owns_worker = *caller != Caller::OtherLineage;
        let mut check = |label: String, want: Want, status: u16, body: &str| {
            if body.contains(SENTINEL) {
                problems.push(format!(
                    "{caller:?} {label}: the owner's words leaked: {body}"
                ));
            }
            let ok = match want {
                Want::Entity => status == 200 && body.contains(&owner),
                Want::Status(wanted) => status == wanted,
                Want::Any => true,
            };
            if !ok {
                problems.push(format!(
                    "{caller:?} {label}: unexpected {status} (the check would be vacuous): {body}"
                ));
            }
        };
        let get = reqwest::Method::GET;
        let post = reqwest::Method::POST;
        macro_rules! sweep {
            ($want:expr, $method:expr, $path:expr) => {
                sweep!($want, $method, $path, None)
            };
            ($want:expr, $method:expr, $path:expr, $body:expr) => {{
                let path: String = $path;
                let (status, body) = ctx.call(credential, &headers, $method, &path, $body).await;
                check(format!("{} {path}", $method), $want, status, &body);
                body
            }};
        }

        // Tasks: the agent_scope task read and write entries.
        sweep!(
            Want::Entity,
            get.clone(),
            format!("/v1/tasks?project={project}")
        );
        sweep!(Want::Entity, get.clone(), format!("/v1/tasks/{task_id}"));
        sweep!(
            Want::Entity,
            get.clone(),
            format!("/v1/board?project={project}")
        );
        sweep!(
            Want::Status(200),
            get.clone(),
            format!("/v1/tasks/{task_id}/placements")
        );
        sweep!(
            Want::Entity,
            post.clone(),
            format!("/v1/tasks/{task_id}/placements"),
            Some(json!({"kind": "dispatch"}))
        );
        sweep!(
            Want::Entity,
            reqwest::Method::PATCH,
            format!("/v1/tasks/{task_id}"),
            Some(json!({"state": "running"}))
        );
        sweep!(
            Want::Status(200),
            post.clone(),
            format!("/v1/tasks/{task_id}/own"),
            Some(json!({"paths": ["src/reqby/**"]}))
        );
        sweep!(
            Want::Status(200),
            get.clone(),
            format!("/v1/own?project={project}")
        );
        sweep!(
            Want::Status(200),
            post.clone(),
            "/v1/own/check".to_string(),
            Some(json!({"taskId": task_id, "paths": ["src/reqby/a.rs"]}))
        );
        sweep!(
            Want::Status(200),
            reqwest::Method::DELETE,
            format!("/v1/tasks/{task_id}/own"),
            Some(json!({"paths": ["src/reqby/**"]}))
        );
        sweep!(
            Want::Status(200),
            post.clone(),
            format!("/v1/tasks/{task_id}/split"),
            Some(json!({"title": "child", "intent": "a part"}))
        );
        sweep!(
            if agent {
                Want::Status(403)
            } else {
                Want::Entity
            },
            post.clone(),
            format!("/v1/tasks/{task_id}/land"),
            Some(json!({"sha": "deadbeef1"}))
        );
        sweep!(
            Want::Entity,
            post.clone(),
            format!("/v1/tasks/{task_id}/archive")
        );
        sweep!(
            Want::Status(200),
            post.clone(),
            "/v1/tasks".to_string(),
            Some(json!({"projectId": project, "title": "mine", "intent": "my own"}))
        );

        // Workers: reads, observe, every per-worker verb and dispatch.
        sweep!(
            Want::Entity,
            get.clone(),
            format!("/v1/workers?project={project}")
        );
        sweep!(
            Want::Entity,
            get.clone(),
            format!("/v1/workers/{worker_id}")
        );
        sweep!(
            Want::Entity,
            post.clone(),
            "/v1/workers/observe".to_string(),
            Some(json!({"projectId": project}))
        );
        sweep!(
            Want::Any,
            get.clone(),
            format!("/v1/workers/{worker_id}/digest")
        );
        sweep!(
            Want::Any,
            get.clone(),
            format!("/v1/hosts/{}/hostcap", ctx.host)
        );
        sweep!(
            Want::Any,
            get.clone(),
            format!("/v1/lineages/{seat}/glance")
        );
        let verb = |entity: bool| {
            if !owns_worker {
                Want::Status(403)
            } else if entity {
                Want::Entity
            } else {
                Want::Status(200)
            }
        };
        sweep!(
            verb(true),
            post.clone(),
            format!("/v1/workers/{worker_id}/nudge"),
            Some(json!({}))
        );
        sweep!(
            verb(true),
            post.clone(),
            format!("/v1/workers/{worker_id}/brief"),
            Some(json!({"content": "more context"}))
        );
        sweep!(
            if !owns_worker {
                Want::Status(403)
            } else if agent {
                // keys stand in for no approval: an Agent is asked once
                Want::Status(409)
            } else {
                Want::Status(200)
            },
            post.clone(),
            format!("/v1/workers/{worker_id}/answer"),
            Some(json!({"key": "enter"}))
        );
        sweep!(
            verb(true),
            post.clone(),
            format!("/v1/workers/{worker_id}/switch-model"),
            Some(json!({"model": "claude-sonnet-5-5"}))
        );
        sweep!(
            verb(true),
            post.clone(),
            format!("/v1/workers/{worker_id}/resume"),
            Some(json!({}))
        );
        let replaced = sweep!(
            verb(true),
            post.clone(),
            format!("/v1/workers/{worker_id}/replace")
        );
        let current = serde_json::from_str::<Value>(&replaced)
            .ok()
            .and_then(|body| body["worker"]["id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| worker_id.clone());
        sweep!(
            verb(false),
            post.clone(),
            format!("/v1/workers/{current}/state"),
            Some(json!({"state": "done", "sha": "deadbeef2"}))
        );
        sweep!(
            verb(false),
            post.clone(),
            format!("/v1/workers/{current}/stop")
        );
        sweep!(
            verb(false),
            post.clone(),
            format!("/v1/workers/{current}/retire"),
            Some(json!({"force": true}))
        );
        sweep!(
            Want::Status(200),
            post.clone(),
            "/v1/workers/dispatch".to_string(),
            Some(json!({
                "projectId": project, "brief": "the caller's own brief",
                "briefName": "brief.md", "harness": "claude",
                "name": format!("reqby-own{index}"),
            }))
        );

        // The approval card source: a Bot reads every card; an Agent is
        // refused while delegated decisions are off.
        sweep!(
            if agent {
                Want::Status(403)
            } else {
                Want::Entity
            },
            get.clone(),
            "/v1/interactions".to_string()
        );
    }
    assert!(
        problems.is_empty(),
        "{} problem(s):\n{}",
        problems.len(),
        problems.join("\n")
    );
    Ok(())
}

/// (c) The owner still reads their words where the PWA shows them — the
/// approval card source, the board (the task detail panel's source) and the
/// task detail — and only there: the task list carries the reference alone.
#[tokio::test]
async fn the_owner_reads_their_words_on_the_card_board_and_task_detail() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let text = format!("{SENTINEL}: dispatch a worker on the other node");
    let owner = ctx.owner_turn_recorded(&mut node, &seat, &text).await;
    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let card = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();
    let (status, task) = ctx.create_task(&token, &project, Value::Null).await;
    assert_eq!(status, 200, "{task}");
    let task_id = task["id"].as_str().context("task id")?.to_owned();

    let expected = json!({"commandId": owner, "excerpt": text});
    let cards = ctx.get("/v1/interactions").await;
    assert_eq!(
        card_in(&cards, &card).context("owner's card")?["requestedBy"],
        expected,
        "the approval card source"
    );
    let board = ctx.get(&format!("/v1/board?project={project}")).await;
    let board_card = board["columns"]
        .as_object()
        .into_iter()
        .flat_map(|columns| columns.values())
        .filter_map(Value::as_array)
        .flatten()
        .find(|card| card["id"] == json!(task_id))
        .cloned()
        .context("the task on the board")?;
    assert_eq!(board_card["requestedBy"], expected, "the board");
    let detail = ctx.get(&format!("/v1/tasks/{task_id}")).await;
    assert_eq!(detail["requestedBy"], expected, "the task detail");
    assert_eq!(
        first_requested_by(&ctx.get(&format!("/v1/tasks?project={project}")).await),
        Some(json!({"commandId": owner})),
        "the task list is not a surface the words are shown on"
    );

    // `requestedBy` is Hub-stamped only: a card a Node lists carries none of
    // its own, even when it names the owner's command, and even for the owner.
    node.cards.lock().expect("cards").push(json!({
        "interactionId": "int_node_card", "id": "int_node_card",
        "instanceId": seat, "hostId": ctx.host, "kind": "approval",
        "state": "pending", "requestedBy": {"commandId": owner},
    }));
    let cards = ctx.get("/v1/interactions").await;
    assert!(
        card_in(&cards, "int_node_card")
            .context("the Node's card")?
            .get("requestedBy")
            .is_none(),
        "{cards}"
    );
    assert_eq!(
        card_in(&cards, &card).context("owner's card")?["requestedBy"],
        expected,
        "the Hub's own card still carries the words"
    );
    Ok(())
}

/// cb-requested-by r4: `requestedBy` is Hub-stamped only. A Node's interaction
/// payload — journaled as `interaction.requested` or listed live — can neither
/// leak nor forge the owner's words: the record is dropped at ingest (never
/// stored) and again from every Node-merged item served, for every caller.
#[tokio::test]
async fn a_node_supplied_requested_by_is_never_stored_or_served() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, _token) = ctx.seat(&mut node, "main").await?;
    let bot = ctx.hub.mint_bot_device_token("reqby-bot").await?;
    let forged = json!({
        "commandId": "cmd_never_issued",
        "excerpt": NODE_SENTINEL,
        "origin": "human",
    });

    // A Node journals an approval whose payload carries its own record.
    let journal_card = "int_node_journal_card";
    ctx.journal(
        &node,
        &seat,
        json!({
            "kind": "interaction.requested",
            "payload": {
                "interactionKind": "approval",
                "requestedBy": forged.clone(),
                "interaction": {
                    "id": journal_card, "kind": "approval", "state": "pending",
                    "blocking": true, "answerable": true, "carrier": "harness-hook",
                    "deadline": {"state": "unknown"}, "resolution": {"state": "unknown"},
                    "request": {"kind": "approval", "title": "Bash",
                                "description": "a Node's own card", "options": []},
                    "requestedBy": forged.clone(),
                }
            }
        }),
    )
    .await;
    // The Node then moves a second card through an entity lifecycle whose
    // entity carries a record: that event overwrites the stored row.
    let entity_card = "int_node_entity_card";
    ctx.journal(
        &node,
        &seat,
        json!({
            "kind": "interaction.requested",
            "payload": {
                "interactionKind": "approval",
                "interaction": {
                    "id": entity_card, "kind": "approval", "state": "pending",
                    "blocking": true, "answerable": true, "carrier": "harness-hook",
                    "deadline": {"state": "unknown"}, "resolution": {"state": "unknown"},
                    "request": {"kind": "approval", "title": "Bash",
                                "description": "a Node's second card", "options": []},
                }
            }
        }),
    )
    .await;
    ctx.journal(
        &node,
        &seat,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity", "entityType": "interaction", "state": "expired",
                "reasonCode": "timed-out",
                "entity": {"id": entity_card, "kind": "approval", "state": "expired",
                           "requestedBy": forged.clone()},
            }
        }),
    )
    .await;
    // A Node lists a live item with its own record, at both levels.
    let list_card = "int_node_list_card";
    node.cards.lock().expect("cards").push(json!({
        "interactionId": list_card, "id": list_card,
        "instanceId": seat, "hostId": ctx.host, "kind": "approval",
        "state": "pending",
        "interaction": {"id": list_card, "kind": "approval", "requestedBy": forged.clone()},
        "requestedBy": forged.clone(),
    }));

    // The owner and a Bot read the page: neither the words nor the record a
    // Node supplied on either card.
    for (who, token) in [("owner", ctx.human.clone()), ("bot", bot.clone())] {
        let (status, body) = ctx
            .call(&token, &[], reqwest::Method::GET, "/v1/interactions", None)
            .await;
        assert_eq!(status, 200, "{who}: {body}");
        assert!(
            !body.contains(NODE_SENTINEL),
            "{who} read the Node's words: {body}"
        );
        let page: Value = serde_json::from_str(&body)?;
        for id in [journal_card, entity_card, list_card] {
            let card = card_in(&page, id).unwrap_or_else(|| panic!("{who}: no card {id}: {body}"));
            assert!(
                card.get("requestedBy").is_none(),
                "{who}: {id} kept a supplied requestedBy: {card}"
            );
            assert!(
                card["interaction"].get("requestedBy").is_none(),
                "{who}: {id} kept a supplied nested requestedBy: {card}"
            );
        }
    }

    // Never persisted: the interaction row and the journal row that carried it
    // hold no record and none of the words.
    let conn = rusqlite::Connection::open(ctx.dir.path().join("data/hub.sqlite"))?;
    let stored: Vec<String> = conn
        .prepare("SELECT payload_json FROM interactions")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert!(!stored.is_empty(), "no interaction row to check");
    for payload in stored {
        assert!(
            !payload.contains("requestedBy") && !payload.contains(NODE_SENTINEL),
            "{payload}"
        );
    }
    let journaled: Vec<String> = conn
        .prepare(
            "SELECT payload_json FROM journal WHERE payload_json LIKE '%interaction.requested%'",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert!(!journaled.is_empty(), "no journal row to check");
    for payload in journaled {
        assert!(
            !payload.contains("requestedBy") && !payload.contains(NODE_SENTINEL),
            "{payload}"
        );
    }
    Ok(())
}

/// cb-requested-by r5: the ingest gate classifies an interaction event exactly
/// as the store does (`kind`, else `subtype`), and the strip walks the whole
/// value. A flat `subtype` event and records nested in `meta` objects and array
/// elements are neither stored nor served — not in the journal row, the
/// interaction row, the raw `/v1/interactions` page or the raw journal read.
#[tokio::test]
async fn a_flat_or_deeply_nested_node_requested_by_is_never_stored_or_served() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, _token) = ctx.seat(&mut node, "main").await?;
    let bot = ctx.hub.mint_bot_device_token("reqby-bot").await?;
    let forged = json!({
        "commandId": "cmd_never_issued",
        "excerpt": NODE_SENTINEL,
        "origin": "human",
    });

    // A flat event the store classifies by `subtype`: no `kind`, no nested
    // interaction object, the record at the top of its payload.
    let flat_card = "int_node_flat_card";
    ctx.journal(
        &node,
        &seat,
        json!({
            "subtype": "interaction.requested",
            "interactionKind": "approval",
            "payload": {
                "interactionId": flat_card,
                "kind": "approval",
                "requestedBy": forged.clone(),
            }
        }),
    )
    .await;
    // A record buried in a `meta` object and inside an array element.
    let nested_card = "int_node_nested_card";
    ctx.journal(
        &node,
        &seat,
        json!({
            "kind": "interaction.requested",
            "payload": {
                "interactionKind": "approval",
                "interaction": {
                    "id": nested_card, "kind": "approval", "state": "pending",
                    "blocking": true, "answerable": true, "carrier": "harness-hook",
                    "deadline": {"state": "unknown"}, "resolution": {"state": "unknown"},
                    "meta": {"requestedBy": forged.clone()},
                    "request": {"kind": "approval", "title": "Bash",
                                "description": "a Node's own card",
                                "options": [{"id": "allow-once", "label": "Allow",
                                             "meta": {"requestedBy": forged.clone()}}]},
                }
            }
        }),
    )
    .await;
    // A live Node item with the same buried records.
    let list_card = "int_node_nested_list_card";
    node.cards.lock().expect("cards").push(json!({
        "interactionId": list_card, "id": list_card,
        "instanceId": seat, "hostId": ctx.host, "kind": "approval", "state": "pending",
        "meta": {"requestedBy": forged.clone()},
        "interaction": {"id": list_card, "kind": "approval",
                        "request": {"options": [{"meta": {"requestedBy": forged.clone()}}]}},
    }));

    // Every leak is collected, so one run names them all.
    let mut leaks = Vec::new();
    for (who, token) in [("owner", ctx.human.clone()), ("bot", bot.clone())] {
        let (status, body) = ctx
            .call(&token, &[], reqwest::Method::GET, "/v1/interactions", None)
            .await;
        assert_eq!(status, 200, "{who}: {body}");
        let page: Value = serde_json::from_str(&body)?;
        for id in [flat_card, nested_card, list_card] {
            let card = card_in(&page, id).unwrap_or_else(|| panic!("{who}: no card {id}: {body}"));
            if card.to_string().contains(NODE_SENTINEL) {
                leaks.push(format!("{who} /v1/interactions card {id}: {card}"));
            }
        }
        let (status, journal) = ctx
            .call(
                &token,
                &[],
                reqwest::Method::GET,
                &format!("/v1/instances/{seat}/journal"),
                None,
            )
            .await;
        assert_eq!(status, 200, "{who}: {journal}");
        let journal: Value = serde_json::from_str(&journal)?;
        let events = journal["events"].as_array().cloned().unwrap_or_default();
        for id in [flat_card, nested_card] {
            let row = events
                .iter()
                .find(|row| row.to_string().contains(id))
                .unwrap_or_else(|| panic!("{who}: the journal read lacks {id}: {journal}"));
            if row.to_string().contains(NODE_SENTINEL) {
                leaks.push(format!("{who} journal read {id}: {row}"));
            }
        }
    }

    let conn = rusqlite::Connection::open(ctx.dir.path().join("data/hub.sqlite"))?;
    for table in ["interactions", "journal"] {
        let rows: Vec<String> = conn
            .prepare(&format!("SELECT payload_json FROM {table}"))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in [flat_card, nested_card] {
            let row = rows
                .iter()
                .find(|row| row.contains(id))
                .unwrap_or_else(|| panic!("no {table} row for {id}"));
            if row.contains("requestedBy") || row.contains(NODE_SENTINEL) {
                leaks.push(format!("stored {table} row {id}: {row}"));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "{} leak(s) of a Node-supplied requestedBy:\n{}",
        leaks.len(),
        leaks.join("\n")
    );
    Ok(())
}
