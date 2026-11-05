//! cb-command-queue (protocol.md §8.5): own queued commands — list, edit,
//! withdraw — and runtime-config generations, over HTTP against the
//! in-process Hub with one scripted fake Node.
//!
//! What only the wiring can show: "own" follows the Hub-stamped initiator
//! across Agents, a Human device and the owner; edit and withdraw act only
//! while the command is provably undelivered (Hub-held, or Node-held through
//! the Node's own answer); a withdraw racing a forward has exactly one
//! winner; an edit never makes a command go out twice (D-055); a stale
//! `expectedGeneration` is a 409 and a mid-turn configure says `deferred`.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, WORKER_DIGEST_MAX_BYTES};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// How the fake Node answers the held-input methods, and what it received.
#[derive(Clone)]
struct NodeScript {
    frames: Arc<Mutex<Vec<Value>>>,
    withdraw: Arc<Mutex<&'static str>>,
    edit: Arc<Mutex<&'static str>>,
}

impl NodeScript {
    fn new() -> Self {
        Self {
            frames: Arc::new(Mutex::new(Vec::new())),
            withdraw: Arc::new(Mutex::new("already-delivered")),
            edit: Arc::new(Mutex::new("already-delivered")),
        }
    }

    /// Request frames of `method` the Node received.
    fn received(&self, method: &str) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame["method"] == method)
            .cloned()
            .collect()
    }

    /// `instance.send` frames carrying `command_id`.
    fn sends_of(&self, command_id: &str) -> Vec<Value> {
        self.received("instance.send")
            .into_iter()
            .filter(|frame| frame["params"]["commandId"] == command_id)
            .collect()
    }
}

struct FakeNode {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return Ok(serde_json::from_str(&text)?),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            other => return Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

/// Hello as a fence- and held-input-capable Node; returns the driver task and
/// the node token a reconnect presents.
async fn connect_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
    workspace: &str,
    script: NodeScript,
) -> Result<(FakeNode, String)> {
    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "capabilities": { "lineageFence": true, "heldInput": true },
                "label": format!("fake-{host_id}"),
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "labels": ["toolchain=rust"],
                    "maxInstances": 32,
                    "resources": { "cpuPct": 5, "memPct": 20 },
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr": {"version": "0.9.0", "socket": "/tmp/fake-herdr.sock"},
                    "workspaces": [{ "workspaceId": workspace, "hostId": host_id,
                                     "root": format!("/tmp/{workspace}") }],
                    "workspaceRevision": 1,
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    let token = hello["result"]["nodeToken"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| bearer.to_string());
    let task = tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            let (Some(id), Some(method)) = (
                frame.get("id").cloned(),
                frame
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ) else {
                continue;
            };
            script.frames.lock().unwrap().push(frame.clone());
            let result = match method.as_str() {
                "command.withdraw" => json!({
                    "commandId": frame["params"]["commandId"],
                    "outcome": *script.withdraw.lock().unwrap(),
                }),
                "command.edit" => {
                    let outcome = *script.edit.lock().unwrap();
                    let expected = frame["params"]["expectedRevision"].as_i64().unwrap_or(0);
                    json!({
                        "commandId": frame["params"]["commandId"],
                        "outcome": outcome,
                        "revision": if outcome == "edited" { expected + 1 } else { expected },
                    })
                }
                _ => json!({ "accepted": true }),
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if node
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok((FakeNode { task }, token))
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
    script: NodeScript,
    node: Option<FakeNode>,
    node_token: String,
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("cmdqueue-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?;
        let script = NodeScript::new();
        let (node, node_token) =
            connect_node(hub.addr, &enroll, &host, &workspace, script.clone()).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            script,
            node: Some(node),
            node_token,
        })
    }

    /// Drop the Node's socket and its registry slot: the host is offline.
    async fn offline(&mut self) {
        self.node.take();
        self.hub.test_disconnect_node(&self.host).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Reconnect the same host with its node token.
    async fn online(&mut self) -> Result<()> {
        let (node, token) = connect_node(
            self.hub.addr,
            &self.node_token,
            &self.host,
            &self.workspace,
            self.script.clone(),
        )
        .await?;
        self.node = Some(node);
        self.node_token = token;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(())
    }

    async fn call(
        &self,
        method: reqwest::Method,
        token: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, value))
    }

    async fn get(&self, token: &str, path: &str) -> Result<(u16, Value)> {
        self.call(reqwest::Method::GET, token, path, None).await
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> Result<(u16, Value)> {
        self.call(reqwest::Method::POST, token, path, Some(body))
            .await
    }

    async fn ok(&self, token: &str, path: &str, body: Value) -> Result<Value> {
        let (status, value) = self.post(token, path, body).await?;
        anyhow::ensure!(status == 200, "POST {path} {status}: {value}");
        Ok(value)
    }

    async fn project(&self) -> Result<String> {
        let created = self
            .ok(
                &self.human,
                "/v1/projects",
                json!({ "name": format!("cmdqueue-{}", remuda_protocol::CommandId::new().as_id()), "members": [
                    {"hostId": self.host, "workspaceId": self.workspace, "role": "primary"}] }),
            )
            .await?;
        Ok(created["id"].as_str().context("project id")?.to_string())
    }

    /// A human-seated coordinator; returns (instance id, agent token).
    async fn coordinator(&self, project: &str) -> Result<(String, String)> {
        let created = self
            .ok(
                &self.human,
                "/v1/instances",
                json!({ "role": "project-coordinator", "projectId": project,
                        "prompt": "coordinate", "driver": "claude-pty" }),
            )
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string();
        let token = self
            .ok(
                &self.human,
                &format!("/v1/instances/{id}/mcp-token"),
                json!({}),
            )
            .await?["token"]
            .as_str()
            .context("token")?
            .to_string();
        Ok((id, token))
    }

    /// A worker the `token` seat created (so it owns it), on `driver`.
    async fn worker(&self, token: &str, project: &str, driver: &str) -> Result<String> {
        let created = self
            .ok(
                token,
                "/v1/instances",
                json!({ "role": "worker", "projectId": project, "prompt": "work",
                        "driver": driver }),
            )
            .await?;
        Ok(created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string())
    }

    /// The exact body a client POSTs for a send, so a same-id re-POST can
    /// replay it byte for byte.
    fn send_body(command_id: &str, text: &str) -> Value {
        json!({
            "commandId": command_id,
            "operation": "instance.send",
            "payload": { "input": { "type": "prompt", "mode": "new-turn",
                                    "blocks": [{ "type": "text", "text": text }] } },
        })
    }

    async fn send(&self, token: &str, instance: &str, text: &str) -> Result<(String, Value)> {
        let command_id = remuda_protocol::CommandId::new().as_id().to_string();
        let body = Self::send_body(&command_id, text);
        let value = self
            .ok(token, &format!("/v1/instances/{instance}/commands"), body)
            .await?;
        Ok((command_id, value))
    }

    async fn list(&self, token: &str, query: &str) -> Result<Vec<Value>> {
        let (status, value) = self.get(token, &format!("/v1/commands?{query}")).await?;
        anyhow::ensure!(status == 200, "list {status}: {value}");
        Ok(value["commands"].as_array().cloned().unwrap_or_default())
    }

    async fn edit(
        &self,
        token: &str,
        command_id: &str,
        expected: i64,
        text: &str,
    ) -> Result<(u16, Value)> {
        self.call(
            reqwest::Method::PATCH,
            token,
            &format!("/v1/commands/{command_id}"),
            Some(json!({ "expectedRevision": expected, "text": text })),
        )
        .await
    }

    async fn withdraw(&self, token: &str, command_id: &str) -> Result<(u16, Value)> {
        self.post(
            token,
            &format!("/v1/commands/{command_id}/withdraw"),
            json!({}),
        )
        .await
    }

    async fn config(&self, token: &str, instance: &str) -> Result<Value> {
        let (status, value) = self
            .get(token, &format!("/v1/instances/{instance}/config"))
            .await?;
        anyhow::ensure!(status == 200, "config {status}: {value}");
        Ok(value)
    }

    async fn journal(&self, instance_id: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        Ok(())
    }
}

fn ids(rows: &[Value]) -> Vec<String> {
    let mut ids: Vec<String> = rows
        .iter()
        .filter_map(|row| row["commandId"].as_str().map(str::to_string))
        .collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// Own = the Hub-stamped initiator: an Agent sees and acts only on what its
/// lineage queued, never on a Human's or another Agent's; a Human device sees
/// its own; the unnarrowed owner sees and withdraws everyone's.
#[tokio::test]
async fn authority_matrix_follows_the_stamp() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (main_id, main) = ctx.coordinator(&project).await?;
    let elsewhere = ctx.project().await?;
    let (_other, other) = ctx.coordinator(&elsewhere).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    ctx.offline().await;

    let (agent_cmd, queued) = ctx.send(&main, &worker, "from main").await?;
    assert_eq!(queued["command"]["forwarded"], json!(false), "{queued}");
    let (human_cmd, _) = ctx.send(&ctx.human, &worker, "from the owner").await?;

    // Main sees only its own.
    let listed = ctx.list(&main, "mine=1&state=queued").await?;
    assert_eq!(ids(&listed), vec![agent_cmd.clone()], "{listed:?}");
    assert_eq!(listed[0]["held"], "hub");
    assert_eq!(listed[0]["revision"], 0);
    // ... even without mine=1: an Agent's view is always its own.
    assert_eq!(
        ids(&ctx.list(&main, "state=queued").await?),
        vec![agent_cmd.clone()]
    );
    // Another Agent sees nothing, and cannot touch either command.
    assert!(ctx.list(&other, "mine=1").await?.is_empty());
    for command in [&agent_cmd, &human_cmd] {
        assert_eq!(ctx.edit(&other, command, 0, "hijack").await?.0, 404);
        assert_eq!(ctx.withdraw(&other, command).await?.0, 404);
    }
    // An Agent never edits or withdraws a Human's command.
    assert_eq!(ctx.edit(&main, &human_cmd, 0, "rewrite").await?.0, 404);
    assert_eq!(ctx.withdraw(&main, &human_cmd).await?.0, 404);
    // The Human device's own view, and the owner's view of everyone's.
    assert_eq!(
        ids(&ctx.list(&ctx.human, "mine=1&state=queued").await?),
        vec![human_cmd.clone()]
    );
    assert_eq!(
        ids(&ctx.list(&ctx.human, "state=queued").await?),
        sorted(vec![agent_cmd.clone(), human_cmd.clone()])
    );
    // Another Human device is a different queuer.
    let laptop = ctx.hub.mint_device_token("cmdqueue-laptop").await?;
    assert!(ctx.list(&laptop, "mine=1").await?.is_empty());
    // A state other than queued is refused, not silently widened.
    assert_eq!(ctx.get(&main, "/v1/commands?state=settled").await?.0, 400);

    // The owner may withdraw Main's command but never edit it.
    assert_eq!(
        ctx.edit(&ctx.human, &agent_cmd, 0, "owner rewrite")
            .await?
            .0,
        404
    );
    // The owner withdraws Main's command; the actor is recorded.
    let (status, withdrawn) = ctx.withdraw(&ctx.human, &agent_cmd).await?;
    assert_eq!(status, 200, "{withdrawn}");
    assert_eq!(withdrawn["command"]["state"], "settled");
    assert_eq!(withdrawn["command"]["settlement"]["outcome"], "withdrawn");
    assert_eq!(
        withdrawn["command"]["settlement"]["withdrawnBy"],
        json!({ "origin": "human" })
    );
    assert!(ctx.list(&main, "mine=1").await?.is_empty());
    // Main withdraws nothing it does not own; its own withdrawal records it.
    let (agent_cmd2, _) = ctx.send(&main, &worker, "second").await?;
    let (status, mine) = ctx.withdraw(&main, &agent_cmd2).await?;
    assert_eq!(status, 200, "{mine}");
    assert_eq!(
        mine["command"]["settlement"]["withdrawnBy"],
        json!({ "origin": "agent", "instanceId": main_id })
    );
    // Both transcripts say the queued send was withdrawn, without its text.
    let (_, journal) = ctx
        .get(&ctx.human, &format!("/v1/instances/{worker}/journal"))
        .await?;
    let notes: Vec<String> = journal["events"]
        .as_array()
        .context("journal events")?
        .iter()
        .map(|row| row["event"].to_string())
        .filter(|event| event.contains("queued-command-withdrawn"))
        .collect();
    assert!(
        notes.iter().any(|note| note.contains(&agent_cmd2)),
        "{notes:?}"
    );
    assert!(
        notes.iter().all(|note| !note.contains("\"second\"")),
        "{notes:?}"
    );
    // Audit: who, which command, its revision; never the prompt text.
    let store = ctx.hub.store().context("store")?;
    let rows = store.audit_for(agent_cmd2.clone()).await?;
    let withdraw = rows
        .iter()
        .find(|row| row["action"] == "command.withdraw")
        .context("withdraw audit row")?;
    assert_eq!(withdraw["detail"]["revision"], 0);
    assert_eq!(withdraw["detail"]["held"], "hub");
    assert_eq!(withdraw["initiator"]["instanceId"], main_id);
    assert!(!withdraw.to_string().contains("second"), "{withdraw}");
    let owner_rows = store.audit_for(agent_cmd.clone()).await?;
    assert!(
        owner_rows
            .iter()
            .any(|row| row["action"] == "command.withdraw" && row["initiator"].is_null()),
        "{owner_rows:?}"
    );
    Ok(())
}

/// Edit keeps the commandId and bumps the revision under a CAS; withdraw is
/// final; after the host returns, a same-id re-POST of the ORIGINAL body
/// forwards exactly the edited text once and never the withdrawn one; a
/// delivered command is 409 `already-delivered`.
#[tokio::test]
async fn edit_and_withdraw_only_while_undelivered_and_nothing_goes_out_twice() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (_main, main) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    ctx.offline().await;

    let (first, _) = ctx.send(&main, &worker, "first").await?;
    let (second, _) = ctx.send(&main, &worker, "second").await?;
    let (status, edited) = ctx.edit(&main, &first, 0, "first, edited").await?;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["command"]["commandId"], first);
    assert_eq!(edited["command"]["revision"], 1);
    assert_eq!(
        edited["command"]["payload"]["input"]["blocks"][0]["text"],
        "first, edited"
    );
    // The CAS holds: a stale revision is a 409 naming the current one.
    let (status, stale) = ctx.edit(&main, &first, 0, "lost update").await?;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["code"], "COMMAND_REVISION_CONFLICT");
    assert_eq!(stale["revision"], 1);
    assert_eq!(stale["reason"], "revision-conflict");
    let (status, withdrawn) = ctx.withdraw(&main, &second).await?;
    assert_eq!(status, 200, "{withdrawn}");
    // Withdrawing again returns the same settled row.
    let (status, again) = ctx.withdraw(&main, &second).await?;
    assert_eq!(status, 200);
    assert_eq!(again["command"]["settlement"]["outcome"], "withdrawn");
    // A withdrawn command cannot be edited.
    let (status, refused) = ctx.edit(&main, &second, 0, "revive").await?;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["code"], "COMMAND_NOT_QUEUED");
    assert_eq!(refused["reason"], "already-delivered");

    ctx.online().await?;
    assert!(
        ctx.script.received("instance.send").is_empty(),
        "G1: no auto-forward"
    );
    let path = format!("/v1/instances/{worker}/commands");
    // The outbox replays what it holds: the original text, same id.
    let replay = ctx
        .ok(&main, &path, Ctx::send_body(&first, "first"))
        .await?;
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(replay["command"]["forwarded"], true, "{replay}");
    let replay2 = ctx
        .ok(&main, &path, Ctx::send_body(&second, "second"))
        .await?;
    assert_eq!(replay2["command"]["settlement"]["outcome"], "withdrawn");
    // Replaying the edited text also names the same command.
    ctx.ok(&main, &path, Ctx::send_body(&first, "first, edited"))
        .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let sent = ctx.script.sends_of(&first);
    assert_eq!(sent.len(), 1, "exactly one delivery: {sent:?}");
    assert_eq!(
        sent[0]["params"]["input"]["blocks"][0]["text"],
        "first, edited"
    );
    assert!(
        ctx.script.sends_of(&second).is_empty(),
        "withdrawn never forwarded"
    );

    // Delivered (accepted by the Node): the Node says it is past its queue.
    let (status, late) = ctx.edit(&main, &first, 1, "too late").await?;
    assert_eq!(status, 409, "{late}");
    assert_eq!(late["code"], "COMMAND_NOT_QUEUED");
    assert_eq!(late["next"]["commandId"], first);
    let (status, late) = ctx.withdraw(&main, &first).await?;
    assert_eq!(status, 409, "{late}");
    assert_eq!(ctx.script.received("command.edit").len(), 1);
    assert_eq!(ctx.script.received("command.withdraw").len(), 1);
    // And the row the Hub keeps still says what went out.
    let (_, row) = ctx
        .get(&ctx.human, &format!("/v1/instances/{worker}/commands"))
        .await?;
    assert!(row.to_string().contains("first, edited"));
    assert_eq!(ctx.script.sends_of(&first).len(), 1);
    Ok(())
}

/// A prompt the Node holds is edited and withdrawn through the Node; the Hub
/// records exactly what the Node confirmed.
#[tokio::test]
async fn node_held_prompts_are_edited_and_withdrawn_through_the_node() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (_main, main) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    let (held, sent) = ctx.send(&main, &worker, "behind a turn").await?;
    assert_eq!(sent["command"]["state"], "accepted", "{sent}");
    let listed = ctx.list(&main, "mine=1").await?;
    assert_eq!(ids(&listed), vec![held.clone()]);
    assert_eq!(listed[0]["held"], "node");

    *ctx.script.edit.lock().unwrap() = "edited";
    let (status, edited) = ctx.edit(&main, &held, 0, "rewritten").await?;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["command"]["revision"], 1);
    let frames = ctx.script.received("command.edit");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["params"]["expectedRevision"], 0);
    assert_eq!(frames[0]["params"]["prompt"], "rewritten");
    // Stale against the Hub's own revision: refused before the Node is asked.
    let (status, stale) = ctx.edit(&main, &held, 0, "stale").await?;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["code"], "COMMAND_REVISION_CONFLICT");
    assert_eq!(stale["revision"], 1);
    assert_eq!(ctx.script.received("command.edit").len(), 1);
    // The Node's own record can still disagree; its answer is the decision.
    *ctx.script.edit.lock().unwrap() = "revision-conflict";
    let (status, stale) = ctx.edit(&main, &held, 1, "raced").await?;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["code"], "COMMAND_REVISION_CONFLICT");
    // Attachments the Node already staged are not editable.
    let (status, refused) = ctx
        .call(
            reqwest::Method::PATCH,
            &main,
            &format!("/v1/commands/{held}"),
            Some(json!({ "expectedRevision": 1, "attachments": [] })),
        )
        .await?;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["reason"], "not-editable");

    *ctx.script.withdraw.lock().unwrap() = "withdrawn";
    let (status, withdrawn) = ctx.withdraw(&main, &held).await?;
    assert_eq!(status, 200, "{withdrawn}");
    assert_eq!(withdrawn["command"]["settlement"]["outcome"], "withdrawn");
    assert!(ctx.list(&main, "mine=1").await?.is_empty());
    Ok(())
}

/// Withdraw racing the same-id forward: every command ends exactly one way —
/// withdrawn and never sent, or sent once and the withdraw refused.
#[tokio::test]
async fn withdraw_against_forward_has_exactly_one_winner() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (_main, main) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    ctx.offline().await;
    let mut commands = Vec::new();
    for n in 0..16 {
        let text = format!("race {n}");
        let (id, _) = ctx.send(&main, &worker, &text).await?;
        commands.push((id, text));
    }
    ctx.online().await?;
    let ctx = Arc::new(ctx);
    let path = format!("/v1/instances/{worker}/commands");
    let mut races = Vec::new();
    for (id, text) in commands.clone() {
        let (a, b) = (ctx.clone(), ctx.clone());
        let (main_a, main_b) = (main.clone(), main.clone());
        let (id_a, id_b, path) = (id.clone(), id.clone(), path.clone());
        races.push(tokio::spawn(async move {
            let withdraw = tokio::spawn(async move { a.withdraw(&main_a, &id_a).await });
            let forward =
                tokio::spawn(
                    async move { b.post(&main_b, &path, Ctx::send_body(&id_b, &text)).await },
                );
            let (withdraw, forward) = (withdraw.await??, forward.await??);
            anyhow::Ok((id, withdraw, forward))
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut withdrawn = 0;
    for race in races {
        let (id, (withdraw_status, _), (forward_status, forward)) = race.await??;
        assert_eq!(forward_status, 200, "{forward}");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let sends = ctx.script.sends_of(&id).len();
        match withdraw_status {
            200 => {
                withdrawn += 1;
                assert_eq!(sends, 0, "{id} withdrawn yet delivered");
                assert_eq!(forward["command"]["settlement"]["outcome"], "withdrawn");
            }
            409 => assert_eq!(sends, 1, "{id} delivered {sends} times"),
            other => panic!("withdraw of {id} answered {other}"),
        }
    }
    // Both sides win sometimes is not asserted (scheduling decides); the
    // invariant is per command.
    let _ = withdrawn;
    Ok(())
}

/// D-055: a queued configure is never edited, only withdrawn (restoring the
/// values it set, a new generation) and never forwarded afterwards.
#[tokio::test]
async fn a_queued_configure_is_withdrawn_never_edited() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (_main, main) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    let before = ctx.config(&main, &worker).await?;
    let generation = before["generation"].as_i64().context("generation")?;
    ctx.offline().await;
    let command_id = remuda_protocol::CommandId::new().as_id().to_string();
    let body = json!({ "commandId": command_id, "operation": "instance.configure",
                       "payload": { "effort": { "name": "high", "ultracode": false } } });
    let path = format!("/v1/instances/{worker}/commands");
    let queued = ctx.ok(&main, &path, body.clone()).await?;
    assert_eq!(queued["command"]["state"], "queued", "{queued}");
    let config = ctx.config(&main, &worker).await?;
    assert_eq!(config["generation"], generation + 1);
    assert_eq!(config["effort"]["name"], "high");
    assert_eq!(config["pending"][0]["commandId"], command_id);
    assert_eq!(config["pending"][0]["state"], "queued");
    let listed = ctx.list(&main, "mine=1").await?;
    assert_eq!(ids(&listed), vec![command_id.clone()]);

    let (status, refused) = ctx.edit(&main, &command_id, 0, "high").await?;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["reason"], "not-editable");
    let (status, withdrawn) = ctx.withdraw(&main, &command_id).await?;
    assert_eq!(status, 200, "{withdrawn}");
    let config = ctx.config(&main, &worker).await?;
    assert_eq!(
        config["effort"], before["effort"],
        "the withdrawn value is restored"
    );
    assert_eq!(
        config["generation"],
        generation + 2,
        "a restore is a new generation"
    );
    assert_eq!(config["pending"], json!([]));

    ctx.online().await?;
    let replay = ctx.ok(&main, &path, body).await?;
    assert_eq!(replay["command"]["settlement"]["outcome"], "withdrawn");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(ctx.script.received("instance.configure").is_empty());
    Ok(())
}

/// Main and the owner write from what they read: the stale writer gets a 409
/// with the current generation and values, persisted with its command; an
/// Agent may configure model and effort on what it owns, nothing else.
#[tokio::test]
async fn a_stale_expected_generation_is_a_conflict_not_an_overwrite() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (_main, main) = ctx.coordinator(&project).await?;
    let elsewhere = ctx.project().await?;
    let (_other, other) = ctx.coordinator(&elsewhere).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    let path = format!("/v1/instances/{worker}/commands");
    let read = ctx.config(&ctx.human, &worker).await?["generation"]
        .as_i64()
        .context("generation")?;

    let mains = ctx
        .ok(
            &main,
            &path,
            json!({ "operation": "instance.configure",
                    "payload": { "effort": { "name": "medium" }, "expectedGeneration": read } }),
        )
        .await?;
    assert_ne!(
        mains["command"]["settlement"]["outcome"], "rejected",
        "{mains}"
    );
    assert_eq!(
        mains["generation"],
        read + 1,
        "the response names the new generation"
    );
    let owner_id = remuda_protocol::CommandId::new().as_id().to_string();
    let owners = json!({ "commandId": owner_id, "operation": "instance.configure",
                         "payload": { "effort": { "name": "high" }, "expectedGeneration": read } });
    let (status, conflict) = ctx.post(&ctx.human, &path, owners.clone()).await?;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["code"], "CONFIG_GENERATION_CONFLICT");
    assert_eq!(conflict["reason"], "generation-conflict");
    assert_eq!(conflict["generation"], read + 1);
    assert_eq!(conflict["config"]["effort"]["name"], "medium");
    let audit = ctx
        .hub
        .store()
        .context("store")?
        .audit_for(worker.clone())
        .await?;
    let row = audit
        .iter()
        .find(|row| row["action"] == "instance.configure.conflict")
        .context("conflict audit row")?;
    assert_eq!(row["detail"]["commandId"], owner_id);
    assert_eq!(row["detail"]["expectedGeneration"], read);
    assert_eq!(row["detail"]["generation"], read + 1);
    // D-055: the persisted outcome replays as the same 409, never a merge.
    let (status, replay) = ctx.post(&ctx.human, &path, owners).await?;
    assert_eq!(status, 409, "{replay}");
    assert_eq!(replay["generation"], read + 1);
    let config = ctx.config(&ctx.human, &worker).await?;
    assert_eq!(config["effort"]["name"], "medium", "Main's write stands");
    assert_eq!(config["generation"], read + 1);

    // An Agent may not change the permission mode, nor configure what it
    // does not own.
    let (status, refused) = ctx
        .post(
            &main,
            &path,
            json!({ "operation": "instance.configure",
                    "payload": { "permissionMode": "acceptEdits" } }),
        )
        .await?;
    assert_eq!(status, 403, "{refused}");
    let (status, refused) = ctx
        .post(
            &other,
            &path,
            json!({ "operation": "instance.configure",
                    "payload": { "effort": { "name": "low" } } }),
        )
        .await?;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["reason"], "not-owner");
    // The other Agent cannot read the worker's config either.
    assert_eq!(
        ctx.get(&other, &format!("/v1/instances/{worker}/config"))
            .await?
            .0,
        403
    );
    Ok(())
}

/// A configure to a busy PTY worker answers `deferred: next-turn`, shows as
/// pending in the config read and the worker digest (within its bound), and
/// stops being pending when the switch's own applied edge is journaled.
#[tokio::test]
async fn a_mid_turn_configure_is_deferred_until_its_turn_boundary() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project().await?;
    let (_main, main) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&main, &project, "claude-pty").await?;
    let roster_id = remuda_protocol::WorkerRosterId::new().as_id().to_string();
    let roster: remuda_protocol::WorkerRoster = serde_json::from_value(json!({
        "id": roster_id, "revision": "1",
        "createdAt": "2026-11-05T01:00:00.000Z", "updatedAt": "2026-11-05T01:00:00.000Z",
        "projectId": project, "name": "w-defer", "instanceId": worker,
        "hostId": ctx.host, "workspaceId": ctx.workspace, "harness": "claude",
        "driver": "claude-pty", "model": "claude-sonnet-5-5",
        "branch": "wt/w-defer/b-defer-md", "worktreePath": "/w/w-defer",
        "state": { "state": "working" },
    }))?;
    ctx.hub
        .store()
        .context("store")?
        .insert_worker(
            roster,
            "cmdqueue-test".into(),
            remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
        )
        .await?;
    ctx.journal(
        &worker,
        json!({"kind":"lifecycle","payload":{"type":"native","topic":"session",
            "nativeName":"session","nativeId":{"state":"known","value":"session-1"},
            "status":{"state":"known","value":"working"},"relatedIds":{},"dataRef":null,
            "severity":"info","affectsCompletion":false}}),
    )
    .await?;
    let (_, instance) = ctx.get(&main, &format!("/v1/instances/{worker}")).await?;
    assert_eq!(instance["activity"], "working", "{instance}");

    let path = format!("/v1/instances/{worker}/commands");
    let configured = ctx
        .ok(
            &main,
            &path,
            json!({ "operation": "instance.configure",
                    "payload": { "effort": { "name": "high", "requestId": "effreq_defer" } } }),
        )
        .await?;
    assert_eq!(
        configured["command"]["deferred"],
        json!({ "boundary": "next-turn" }),
        "{configured}"
    );
    let config = ctx.config(&main, &worker).await?;
    assert_eq!(config["pending"][0]["state"], "deferred", "{config}");
    assert_eq!(config["pending"][0]["boundary"], "next-turn");
    let generation = config["generation"].as_u64().context("generation")?;

    let (status, digest) = ctx
        .get(&main, &format!("/v1/workers/{roster_id}/digest"))
        .await?;
    assert_eq!(status, 200, "{digest}");
    assert!(digest.to_string().len() <= WORKER_DIGEST_MAX_BYTES);
    assert_eq!(digest["model"]["generation"], generation);
    assert_eq!(
        digest["model"]["configure"]["state"], "deferred",
        "{digest}"
    );
    assert_eq!(digest["model"]["configure"]["boundary"], "next-turn");
    assert_eq!(digest["model"]["configure"]["requestId"], "effreq_defer");

    // The driver's ready ladder applies it at the next idle moment.
    ctx.journal(
        &worker,
        json!({ "kind": "lifecycle", "payload": {
            "type": "native", "nativeName": "instance.configure",
            "status": "effort-applied:high", "severity": "info",
            "relatedIds": { "effortRequestId": "effreq_defer" } } }),
    )
    .await?;
    let config = ctx.config(&main, &worker).await?;
    assert_eq!(config["pending"], json!([]), "{config}");
    let (_, digest) = ctx
        .get(&main, &format!("/v1/workers/{roster_id}/digest"))
        .await?;
    assert!(digest["model"].get("configure").is_none(), "{digest}");
    Ok(())
}

/// claude-print and claude-sdk take effort only at launch: the Hub settles an
/// effort-only configure `next-chapter` itself and strips effort from a mixed
/// one instead of forwarding a change the carrier must refuse.
#[tokio::test]
async fn launch_only_effort_is_deferred_to_the_next_chapter() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let created = ctx
        .ok(
            &ctx.human,
            "/v1/instances",
            json!({ "hostId": ctx.host, "workspaceId": ctx.workspace, "kind": "claude",
                    "driver": "claude-sdk", "prompt": "hi" }),
        )
        .await?;
    let sdk = created["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_string();
    let path = format!("/v1/instances/{sdk}/commands");
    let effort_only = ctx
        .ok(
            &ctx.human,
            &path,
            json!({ "operation": "instance.configure",
                    "payload": { "effort": { "name": "high" } } }),
        )
        .await?;
    assert_eq!(effort_only["command"]["state"], "settled", "{effort_only}");
    assert_eq!(effort_only["command"]["settlement"]["outcome"], "completed");
    assert_eq!(
        effort_only["command"]["deferred"]["boundary"],
        "next-chapter"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.script.received("instance.configure").is_empty());
    let config = ctx.config(&ctx.human, &sdk).await?;
    assert_eq!(
        config["effort"]["name"], "high",
        "the next chapter launches with it"
    );
    assert_eq!(config["pending"][0]["boundary"], "next-chapter");

    let mixed = ctx
        .ok(
            &ctx.human,
            &path,
            json!({ "operation": "instance.configure",
                    "payload": { "model": "claude-opus-5-5", "effort": { "name": "low" } } }),
        )
        .await?;
    assert_eq!(
        mixed["command"]["deferred"]["boundary"], "next-chapter",
        "{mixed}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let frames = ctx.script.received("instance.configure");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["params"]["model"], "claude-opus-5-5");
    assert!(frames[0]["params"].get("effort").is_none(), "{frames:?}");
    Ok(())
}
