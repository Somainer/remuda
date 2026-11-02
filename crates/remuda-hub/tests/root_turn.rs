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
    task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    async fn connect(hub: &remuda_hub::RunningHub, host: &str, workspace: &str) -> Result<Self> {
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
                    "maxInstances": 8,
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
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
}

impl Ctx {
    async fn boot() -> Result<(Self, FakeNode)> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let human = hub.mint_device_token("rootturn-phone").await?;
        let host = HostId::new().as_id().as_str().to_owned();
        let workspace = remuda_protocol::WorkspaceId::new()
            .as_id()
            .as_str()
            .to_owned();
        let node = FakeNode::connect(&hub, &host, &workspace).await?;
        Ok((
            Self {
                _dir: dir,
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

    /// `GET path` as `token`, asserting 200.
    async fn get_with(&self, token: &str, path: &str) -> Value {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .expect("http");
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 200, "GET {path}: {body}");
        body
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
        let (status, body) = self
            .post(
                &self.human,
                "/v1/projects",
                &[],
                json!({
                    "name": "reqby-project",
                    "members": [{
                        "hostId": self.host, "workspaceId": self.workspace, "role": "build"
                    }],
                    "hosts": [{
                        "hostId": self.host, "maxInstances": 8, "maxBuilding": 4,
                        "diskBudgetGb": 10, "portBlocks": ["58600-58629"],
                        "requires": [], "latencyClass": "remote"
                    }],
                }),
            )
            .await;
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
    json!({"kind": "message", "payload": {
        "operation": "open", "messageId": format!("msg_{command}"), "role": "user",
        "phase": "input", "parentToolCallId": null, "commandId": command,
        "blocks": [{"type": "text", "text": "input"}]
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

// ── cb-requested-by: the owner's words behind an Agent dispatch or task ─────

/// An Agent's worker dispatch in an owner turn records the owner's command and
/// words on the approval card it opens and on the roster row it finally
/// writes. A body-supplied `requestedBy` is ignored: the record comes from the
/// Hub's own command row.
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
    let id = body["worker"]["id"].as_str().context("worker id")?;
    let stored = ctx.get(&format!("/v1/workers/{id}")).await;
    assert_eq!(
        stored["requestedBy"], expected,
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
    // Assembled at run time so no source line reads as a token.
    let secret = ["sk", "-live-abcd1234efgh5678ijkl"].concat();
    let long = format!("他说：{} {}", "继续".repeat(200), secret);
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

/// An Agent-created task records the owner's words on its `doc_json`, and the
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

/// The owner's own words are served to Human callers only: every task, board,
/// worker and approval read an Agent or Bot can make keeps the command id and
/// drops the excerpt (cb-requested-by privacy).
#[tokio::test]
async fn owner_words_are_human_only_on_task_worker_and_card_reads() -> Result<()> {
    let (ctx, mut node) = Ctx::boot().await?;
    let (seat, token) = ctx.seat(&mut node, "main").await?;
    let project = ctx.create_project().await;
    let owner = ctx
        .owner_turn_text(&mut node, &seat, "the owner's private words")
        .await;
    let expected = json!({"commandId": owner, "excerpt": "the owner's private words"});
    let stripped = json!({"commandId": owner});

    // An Agent-created task and an Agent dispatch, both recorded in the owner
    // turn, plus a pending card from the same turn.
    let (status, task) = ctx.create_task(&token, &project, Value::Null).await;
    assert_eq!(status, 200, "{task}");
    let task_id = task["id"].as_str().context("task id")?.to_owned();
    let (status, sent) = ctx.worker_dispatch(&token, &project, &[]).await;
    assert_eq!(status, 200, "{sent}");
    let worker_id = sent["worker"]["id"]
        .as_str()
        .context("worker id")?
        .to_owned();
    assert_eq!(
        sent["worker"]["requestedBy"], stripped,
        "the dispatch response itself is stripped for an Agent caller: {sent}"
    );
    let (status, pending) = ctx
        .worker_dispatch(&token, &project, &[("x-remuda-require-approval", "true")])
        .await;
    assert_eq!(status, 409, "{pending}");
    let approval = pending["interactionId"]
        .as_str()
        .context("interactionId")?
        .to_owned();

    let paths = [
        format!("/v1/tasks?project={project}"),
        format!("/v1/tasks/{task_id}"),
        format!("/v1/board?project={project}"),
        format!("/v1/workers?project={project}"),
        format!("/v1/workers/{worker_id}"),
    ];
    for path in &paths {
        let body = ctx.get_with(&token, path).await;
        let found = first_requested_by(&body)
            .unwrap_or_else(|| panic!("no requestedBy in the Agent read of {path}: {body}"));
        assert_eq!(found, stripped, "Agent read of {path}");
        assert!(
            !body.to_string().contains("private words"),
            "the words never reach an Agent on {path}: {body}"
        );
    }

    // A Bot relays cards to a chat platform: same strip, by deliberate choice.
    let bot = ctx.hub.mint_bot_device_token("reqby-bot").await?;
    let bot_cards = ctx.get_with(&bot, "/v1/interactions").await;
    let bot_card =
        card_in(&bot_cards, &approval).unwrap_or_else(|| panic!("bot card: {bot_cards}"));
    assert_eq!(bot_card["requestedBy"], stripped, "{bot_card}");
    assert!(
        !bot_cards.to_string().contains("private words"),
        "{bot_cards}"
    );

    // The owner sees the words on every one of those reads.
    for path in &paths {
        let body = ctx.get(path).await;
        assert_eq!(
            first_requested_by(&body).expect("human read"),
            expected,
            "Human read of {path}"
        );
    }
    let human_cards = ctx.get("/v1/interactions").await;
    assert_eq!(
        card_in(&human_cards, &approval).expect("human card")["requestedBy"],
        expected,
        "{human_cards}"
    );
    Ok(())
}
