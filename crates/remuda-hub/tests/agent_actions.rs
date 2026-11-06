//! ap-actions (main-agent.md §15.2): three Agent action gaps closed —
//! `ap-configure` (model, effort and ultracode on owned targets),
//! `ap-resume` (an owned instance that is not a worker) and `ap-attachments`
//! (upload, and reading an owned target's attachments) — over HTTP against
//! the in-process Hub with two scripted fake Nodes.
//!
//! Each item is checked as an authority matrix: owned, foreign (another
//! session in the caller's project) and out-of-scope (another project)
//! targets, for an Agent credential, a Human, a Bot and a Human narrowed to
//! the Agent with `x-remuda-instance-id`.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// The per-file attachment cap these Hubs run with.
const ATTACHMENT_MAX: usize = 64 * 1024;

/// Request frames a fake Node received.
#[derive(Clone, Default)]
struct Frames(Arc<Mutex<Vec<Value>>>);

impl Frames {
    fn of(&self, method: &str) -> Vec<Value> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame["method"] == method)
            .cloned()
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

/// Hello as a fence-capable Node that accepts every request.
async fn connect_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
    workspace: &str,
    frames: Frames,
) -> Result<FakeNode> {
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
                "capabilities": { "lineageFence": true, "heldInput": true, "memoryContext": true },
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
    recv_json(&mut node).await?;
    let task = tokio::spawn(async move {
        // No read timeout here: a host that hears nothing for a while must
        // stay online, however slow a loaded test run is.
        while let Some(Ok(message)) = node.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let (Some(id), Some(_)) = (frame.get("id").cloned(), frame.get("method")) else {
                continue;
            };
            frames.0.lock().unwrap().push(frame.clone());
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "accepted": true } });
            if node
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(FakeNode { task })
}

/// One Hub, two hosts, the owner's devices, and two projects: `project` holds
/// both hosts and Main's seat, `elsewhere` holds host A and another seat.
struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    bot: String,
    host_a: String,
    host_b: String,
    ws_a: String,
    frames_a: Frames,
    frames_b: Frames,
    _nodes: Vec<FakeNode>,
    project: String,
    elsewhere: String,
}

/// A seat: its instance id and its Agent credential.
struct Seat {
    id: String,
    token: String,
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let mut config = HubConfig::for_test(dir.path().join("hub"));
        config.attachment_max_bytes = ATTACHMENT_MAX;
        let hub = spawn(config).await?;
        let human = hub.mint_device_token("apactions-human").await?;
        let bot = hub.mint_bot_device_token("apactions-bot").await?;
        let mut nodes = Vec::new();
        let mut hosts = Vec::new();
        let mut frames = Vec::new();
        for _ in 0..2 {
            let host = HostId::new().as_id().to_string();
            let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
            let enroll = hub
                .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
                .await?;
            let seen = Frames::default();
            nodes.push(connect_node(hub.addr, &enroll, &host, &workspace, seen.clone()).await?);
            hosts.push((host, workspace));
            frames.push(seen);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut ctx = Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            bot,
            host_a: hosts[0].0.clone(),
            host_b: hosts[1].0.clone(),
            ws_a: hosts[0].1.clone(),
            frames_a: frames[0].clone(),
            frames_b: frames[1].clone(),
            _nodes: nodes,
            project: String::new(),
            elsewhere: String::new(),
        };
        let members = |pairs: &[(String, String)]| {
            pairs
                .iter()
                .enumerate()
                .map(|(index, (host, workspace))| {
                    json!({"hostId": host, "workspaceId": workspace,
                           "role": if index == 0 { "primary" } else { "member" }})
                })
                .collect::<Vec<_>>()
        };
        ctx.project = ctx.new_project(members(&hosts)).await?;
        ctx.elsewhere = ctx.new_project(members(&hosts[..1])).await?;
        Ok(ctx)
    }

    async fn new_project(&self, members: Vec<Value>) -> Result<String> {
        let created = self
            .ok(
                &self.human,
                "/v1/projects",
                json!({ "name": format!("apactions-{}", remuda_protocol::CommandId::new().as_id()),
                        "members": members }),
            )
            .await?;
        Ok(created["id"].as_str().context("project id")?.to_string())
    }

    async fn call(
        &self,
        method: reqwest::Method,
        token: &str,
        narrow: Option<&str>,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(instance) = narrow {
            request = request.header("x-remuda-instance-id", instance);
        }
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
        self.call(reqwest::Method::GET, token, None, path, None)
            .await
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> Result<(u16, Value)> {
        self.call(reqwest::Method::POST, token, None, path, Some(body))
            .await
    }

    async fn ok(&self, token: &str, path: &str, body: Value) -> Result<Value> {
        let (status, value) = self.post(token, path, body).await?;
        anyhow::ensure!(status == 200, "POST {path} {status}: {value}");
        Ok(value)
    }

    /// A Human-seated coordinator of `project` on host A.
    async fn seat(&self, project: &str) -> Result<Seat> {
        let created = self
            .ok(
                &self.human,
                "/v1/instances",
                json!({ "role": "project-coordinator", "projectId": project,
                        "hostId": self.host_a, "prompt": "coordinate",
                        "driver": "claude-pty" }),
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
        Ok(Seat { id, token })
    }

    /// An instance `token` creates in `project` on `host` (so an Agent caller
    /// owns it).
    async fn child(&self, token: &str, project: &str, host: &str) -> Result<String> {
        let created = self
            .ok(
                token,
                "/v1/instances",
                json!({ "projectId": project, "hostId": host, "prompt": "work",
                        "driver": "claude-pty", "model": "claude-sonnet-5-5" }),
            )
            .await?;
        Ok(created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string())
    }

    async fn configure(
        &self,
        token: &str,
        narrow: Option<&str>,
        instance: &str,
        payload: Value,
    ) -> Result<(u16, Value)> {
        self.call(
            reqwest::Method::POST,
            token,
            narrow,
            &format!("/v1/instances/{instance}/commands"),
            Some(json!({ "operation": "instance.configure", "payload": payload })),
        )
        .await
    }

    async fn generation(&self, instance: &str) -> Result<i64> {
        let (status, config) = self
            .get(&self.human, &format!("/v1/instances/{instance}/config"))
            .await?;
        anyhow::ensure!(status == 200, "config {status}: {config}");
        config["generation"].as_i64().context("generation")
    }

    async fn journal(&self, host: &str, instance: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(host.to_string(), instance.to_string(), None, event)
            .await?;
        Ok(())
    }

    async fn audit(&self, subject: &str) -> Result<Vec<Value>> {
        Ok(self
            .hub
            .store()
            .context("store")?
            .audit_for(subject.to_string())
            .await?)
    }
}

impl Ctx {
    /// A seat seated by the owner with `mode`.
    async fn seat_with_mode(&self, project: &str, mode: &str) -> Result<Seat> {
        let created = self
            .ok(
                &self.human,
                "/v1/instances",
                json!({ "role": "project-coordinator", "projectId": project,
                        "hostId": self.host_a, "prompt": "coordinate",
                        "driver": "claude-pty", "permissionMode": mode }),
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
        Ok(Seat { id, token })
    }

    /// POST as `token`, optionally narrowed and carrying an approval id.
    async fn post_as(
        &self,
        token: &str,
        narrow: Option<&str>,
        approval: Option<&str>,
        path: &str,
        body: Value,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .post(format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token)
            .json(&body);
        if let Some(instance) = narrow {
            request = request.header("x-remuda-instance-id", instance);
        }
        if let Some(id) = approval {
            request = request.header("x-remuda-approval-id", id);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        ))
    }

    /// Create through a D-017 card the owner approves.
    async fn create_approved(&self, token: &str, body: Value) -> Result<String> {
        let (status, asked) = self
            .post_as(token, None, None, "/v1/instances", body.clone())
            .await?;
        anyhow::ensure!(status == 409, "expected a card: {asked}");
        let card = asked["interactionId"].as_str().context("card")?.to_string();
        self.decide(&card, "allow-once").await?;
        let (status, created) = self
            .post_as(token, None, Some(&card), "/v1/instances", body)
            .await?;
        anyhow::ensure!(status == 200, "approved create {status}: {created}");
        Ok(created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string())
    }

    /// The owner answers a D-017 card.
    async fn decide(&self, card: &str, option: &str) -> Result<()> {
        let (_, pending) = self.get(&self.human, "/v1/interactions").await?;
        let item = pending["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["id"] == card || item["interactionId"] == card)
            .with_context(|| format!("no card {card}: {pending}"))?;
        let digest = item
            .pointer("/request/inputDigest")
            .or_else(|| item.pointer("/interaction/request/inputDigest"))
            .and_then(Value::as_str)
            .context("digest")?
            .to_owned();
        let reply = self
            .ok(
                &self.human,
                &format!("/v1/interactions/{card}/answer"),
                json!({ "answer": { "kind": "approval", "optionId": option, "inputDigest": digest } }),
            )
            .await?;
        let _ = reply;
        Ok(())
    }

    /// The instance reported `session` and then exited (D-026's preconditions).
    async fn ended_session(&self, host: &str, instance: &str, session: &str) -> Result<()> {
        self.journal(
            host,
            instance,
            json!({ "kind": "lifecycle", "payload": {
                "type": "native", "topic": "session", "nativeName": "session",
                "nativeId": { "state": "known", "value": session },
                "status": { "state": "known", "value": "started" },
                "relatedIds": { "transcriptPath": format!("/t/{session}.jsonl") },
                "dataRef": null, "severity": "info", "affectsCompletion": false } }),
        )
        .await?;
        self.journal(
            host,
            instance,
            json!({ "kind": "lifecycle",
                    "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" } }),
        )
        .await
    }

    async fn respawn(
        &self,
        token: &str,
        narrow: Option<&str>,
        approval: Option<&str>,
        instance: &str,
        body: Value,
    ) -> Result<(u16, Value)> {
        self.post_as(
            token,
            narrow,
            approval,
            &format!("/v1/instances/{instance}/respawn"),
            body,
        )
        .await
    }

    /// The instance row (as JSON) and its stored spec.
    async fn record(&self, instance: &str) -> Result<(Value, Value)> {
        let store = self.hub.store().context("store")?;
        let record = serde_json::to_value(
            store
                .get_instance(instance.to_string())
                .await?
                .context("instance")?,
        )?;
        let spec = store
            .get_instance_spec_json(instance.to_string())
            .await?
            .unwrap_or(Value::Null);
        Ok((record, spec))
    }
}

fn assert_refused(status: u16, body: &Value, code: u16, reason: &str) {
    assert_eq!(status, code, "{body}");
    assert_eq!(body["reason"], reason, "{body}");
}

// ── ap-configure ───────────────────────────────────────────────────────────

/// Model, effort and ultracode on owned targets for an Agent (and a Human
/// narrowed to it); never on a foreign or out-of-scope target; Humans and
/// Bots unchanged. A permission mode, or any other field, is refused for
/// Agent origin naming `ap-configure-mode`; argv and junk model ids are
/// refused; the audit row names the Agent actor.
#[tokio::test]
async fn configure_authority_matrix() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main = ctx.seat(&ctx.project).await?;
    let other = ctx.seat(&ctx.elsewhere).await?;
    let owned = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    // Foreign: in Main's project (on its other host), created by the owner.
    let foreign = ctx.child(&ctx.human, &ctx.project, &ctx.host_b).await?;
    // Out of scope: another project's seat created it.
    let outside = ctx.child(&other.token, &ctx.elsewhere, &ctx.host_a).await?;
    let model = json!({ "model": "claude-opus-5-5[1m]" });
    let effort = json!({ "effort": { "name": "low", "ultracode": true, "axis": "tier" } });

    // Agent credential, and a Human narrowed to the Agent: owned only.
    for (token, narrow) in [
        (main.token.as_str(), None),
        (ctx.human.as_str(), Some(main.id.as_str())),
    ] {
        for payload in [&model, &effort] {
            let (status, body) = ctx
                .configure(token, narrow, &owned, payload.clone())
                .await?;
            assert_eq!(status, 200, "{narrow:?} {payload}: {body}");
            assert_ne!(body["command"]["settlement"]["outcome"], "rejected");
        }
        for target in [&foreign, &outside] {
            let (status, body) = ctx.configure(token, narrow, target, model.clone()).await?;
            assert_refused(status, &body, 403, "not-owner");
        }
        let (status, body) = ctx
            .configure(
                token,
                narrow,
                &owned,
                json!({ "permissionMode": "acceptEdits" }),
            )
            .await?;
        assert_refused(status, &body, 403, "configure-operator-only");
        assert_eq!(body["next"]["action"], "ask-owner");
        assert!(
            body["next"]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("ap-configure-mode")),
            "{body}"
        );
        // Any other field too, even beside an admitted one.
        let (status, body) = ctx
            .configure(
                token,
                narrow,
                &owned,
                json!({ "model": "claude-opus-5-5", "sandbox": "off" }),
            )
            .await?;
        assert_refused(status, &body, 403, "configure-operator-only");
        assert!(body["error"].as_str().unwrap_or("").contains("sandbox"));
        // Never argv or an executable.
        let (status, body) = ctx
            .configure(
                token,
                narrow,
                &owned,
                json!({ "args": ["--dangerously-skip-permissions"] }),
            )
            .await?;
        assert_refused(status, &body, 403, "operator-only");
        // The create vocabulary for the model id.
        for junk in [
            json!("--dangerously-skip-permissions"),
            json!("opus\u{1b}[A"),
            json!("claude opus"),
            json!(""),
            json!("m".repeat(129)),
            json!(7),
        ] {
            let (status, body) = ctx
                .configure(token, narrow, &owned, json!({ "modelId": junk }))
                .await?;
            assert_refused(status, &body, 400, "model-invalid");
        }
        // The refusals merged nothing.
        let config = ctx
            .get(&ctx.human, &format!("/v1/instances/{owned}/config"))
            .await?
            .1;
        assert_eq!(config["model"], "claude-opus-5-5[1m]", "{config}");
    }
    // The same vocabulary refuses the same id on an Agent create.
    let (status, body) = ctx
        .post(
            &main.token,
            "/v1/instances",
            json!({ "projectId": ctx.project, "hostId": ctx.host_a, "prompt": "work",
                    "driver": "claude-pty", "model": "--dangerously-skip-permissions" }),
        )
        .await?;
    assert_refused(status, &body, 400, "model-invalid");

    // Human and Bot callers are unchanged: any target, and the mode.
    for token in [&ctx.human, &ctx.bot] {
        for target in [&owned, &foreign, &outside] {
            let (status, body) = ctx.configure(token, None, target, effort.clone()).await?;
            assert_eq!(status, 200, "{body}");
        }
        let (status, body) = ctx
            .configure(token, None, &foreign, json!({ "permissionMode": "plan" }))
            .await?;
        assert_eq!(status, 200, "{body}");
    }

    // The audit rows name the Agent actor, for the credential and the
    // narrowed Human alike; Human configures add none.
    let rows: Vec<Value> = ctx
        .audit(&owned)
        .await?
        .into_iter()
        .filter(|row| row["action"] == "instance.configure")
        .collect();
    assert_eq!(rows.len(), 4, "{rows:?}");
    for row in &rows {
        assert_eq!(row["initiator"]["instanceId"], main.id.as_str(), "{row}");
        assert!(row["detail"]["commandId"].is_string(), "{row}");
        assert!(row["detail"]["generation"].is_i64(), "{row}");
    }
    assert_eq!(rows[0]["detail"]["fields"], json!(["model"]));
    assert_eq!(rows[1]["detail"]["fields"], json!(["effort"]));
    assert!(
        ctx.audit(&foreign)
            .await?
            .iter()
            .all(|row| row["action"] != "instance.configure"),
        "Human and Bot configures are not Agent audit rows"
    );
    Ok(())
}

/// The cb-command-queue generation CAS and `deferred` apply to an Agent's
/// configure as to a Human's: a stale generation is a 409 audit row naming
/// the Agent, and a mid-turn switch on a PTY carrier answers `deferred`.
#[tokio::test]
async fn an_agent_configure_keeps_the_generation_cas_and_deferral() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main = ctx.seat(&ctx.project).await?;
    let owned = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    let read = ctx.generation(&owned).await?;
    // The owner writes first; Main's write from the same read is stale.
    let (status, body) = ctx
        .configure(
            &ctx.human,
            None,
            &owned,
            json!({ "effort": { "name": "high" }, "expectedGeneration": read }),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (status, conflict) = ctx
        .configure(
            &main.token,
            None,
            &owned,
            json!({ "model": "claude-opus-5-5", "expectedGeneration": read }),
        )
        .await?;
    assert_refused(status, &conflict, 409, "generation-conflict");
    assert_eq!(conflict["code"], "CONFIG_GENERATION_CONFLICT");
    assert_eq!(conflict["generation"], read + 1);
    let row = ctx
        .audit(&owned)
        .await?
        .into_iter()
        .find(|row| row["action"] == "instance.configure.conflict")
        .context("conflict audit row")?;
    assert_eq!(row["initiator"]["instanceId"], main.id.as_str(), "{row}");
    assert_eq!(ctx.generation(&owned).await?, read + 1, "nothing merged");

    // Mid-turn: the PTY carrier applies it at its next idle moment.
    ctx.journal(
        &ctx.host_a,
        &owned,
        json!({"kind":"lifecycle","payload":{"type":"native","topic":"session",
            "nativeName":"session","nativeId":{"state":"known","value":"session-1"},
            "status":{"state":"known","value":"working"},"relatedIds":{},"dataRef":null,
            "severity":"info","affectsCompletion":false}}),
    )
    .await?;
    let (status, body) = ctx
        .configure(
            &main.token,
            None,
            &owned,
            json!({ "model": "claude-opus-5-5", "expectedGeneration": read + 1 }),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["command"]["deferred"],
        json!({ "boundary": "next-turn" }),
        "{body}"
    );
    assert_eq!(body["generation"], read + 2);
    assert_eq!(
        ctx.frames_a.of("instance.configure").len(),
        2,
        "both admitted configures reached host A"
    );
    assert!(ctx.frames_b.of("instance.configure").is_empty());
    Ok(())
}

// ── ap-resume ──────────────────────────────────────────────────────────────

/// Insert a roster worker whose instance is `instance`.
async fn roster_worker(ctx: &Ctx, instance: &str) -> Result<String> {
    let roster_id = remuda_protocol::WorkerRosterId::new().as_id().to_string();
    let roster: remuda_protocol::WorkerRoster = serde_json::from_value(json!({
        "id": roster_id, "revision": "1",
        "createdAt": "2026-11-06T01:00:00.000Z", "updatedAt": "2026-11-06T01:00:00.000Z",
        "projectId": ctx.project, "name": format!("w-{}", &roster_id[roster_id.len() - 6..]),
        "instanceId": instance,
        "hostId": ctx.host_a, "workspaceId": ctx.ws_a, "harness": "claude",
        "driver": "claude-pty", "model": "claude-sonnet-5-5",
        "branch": "wt/w-resume/b-resume-md", "worktreePath": "/w/w-resume",
        "state": { "state": "working" },
    }))?;
    ctx.hub
        .store()
        .context("store")?
        .insert_worker(
            roster,
            "apactions-test".into(),
            remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
        )
        .await?;
    Ok(roster_id)
}

/// An owned instance that is not a worker resumes on its own host with no
/// card, for the Agent credential and a Human narrowed to it; the new
/// instance stays the caller's (parent = the creating chapter). Foreign and
/// out-of-scope targets, a worker's instance, the seat's own chapter and a
/// seat without `dispatch` are refused; Humans and Bots keep D-026.
#[tokio::test]
async fn resume_authority_matrix() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main = ctx.seat(&ctx.project).await?;
    let other = ctx.seat(&ctx.elsewhere).await?;
    let owned = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    let again = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    let foreign = ctx.child(&ctx.human, &ctx.project, &ctx.host_a).await?;
    let outside = ctx.child(&other.token, &ctx.elsewhere, &ctx.host_a).await?;
    let worker_instance = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    let roster_id = roster_worker(&ctx, &worker_instance).await?;
    for (index, id) in [&owned, &again, &foreign, &outside, &worker_instance]
        .into_iter()
        .enumerate()
    {
        ctx.ended_session(&ctx.host_a, id, &format!("session-{index}"))
            .await?;
    }

    let (status, body) = ctx
        .respawn(&main.token, None, None, &owned, json!({}))
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replayed"], false);
    let resumed = body["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_string();
    let (record, spec) = ctx.record(&resumed).await?;
    assert_eq!(record["parentInstanceId"], main.id.as_str(), "{record}");
    assert_eq!(record["resumedFrom"], owned.as_str());
    assert_eq!(record["hostId"], ctx.host_a.as_str());
    assert_eq!(spec["resumeSessionId"], "session-0");
    assert_eq!(spec["origin"], "agent");
    assert_eq!(ctx.frames_a.of("instance.resume").len(), 1);
    // The caller still owns what it resumed.
    assert_eq!(
        ctx.get(&main.token, &format!("/v1/instances/{resumed}"))
            .await?
            .0,
        200
    );
    // A repeat inside the window replays the same instance.
    let (status, body) = ctx
        .respawn(&main.token, None, None, &owned, json!({}))
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replayed"], true);
    assert_eq!(body["instance"]["instanceId"], resumed.as_str());
    assert_eq!(ctx.frames_a.of("instance.resume").len(), 1);
    // A Human narrowed to Main is Main.
    let (status, body) = ctx
        .respawn(
            &ctx.human,
            Some(&main.id),
            None,
            &again,
            json!({ "mode": "terminal" }),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["instance"]["driver"], "claude-pty");
    let audit = ctx.audit(&again).await?;
    let row = audit
        .iter()
        .find(|row| row["action"] == "instance.respawn")
        .context("respawn audit row")?;
    assert_eq!(row["initiator"]["instanceId"], main.id.as_str(), "{row}");

    for (token, narrow) in [
        (main.token.as_str(), None),
        (ctx.human.as_str(), Some(main.id.as_str())),
    ] {
        for target in [&foreign, &outside] {
            let (status, body) = ctx.respawn(token, narrow, None, target, json!({})).await?;
            assert_eq!(status, 403, "{target}: {body}");
        }
        let (status, body) = ctx
            .respawn(token, narrow, None, &worker_instance, json!({}))
            .await?;
        assert_refused(status, &body, 400, "worker-instance");
        assert!(
            body["next"]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains(&roster_id)),
            "{body}"
        );
        let (status, body) = ctx
            .respawn(token, narrow, None, &main.id, json!({}))
            .await?;
        assert_refused(status, &body, 403, "operator-only");
    }
    // A seat without `dispatch`.
    let leaf = ctx
        .ok(
            &ctx.human,
            &format!("/v1/instances/{owned}/mcp-token"),
            json!({}),
        )
        .await?["token"]
        .as_str()
        .context("token")?
        .to_string();
    let (status, body) = ctx.respawn(&leaf, None, None, &owned, json!({})).await?;
    assert_refused(status, &body, 403, "grant-required");
    assert_eq!(ctx.frames_a.of("instance.resume").len(), 2);

    // D-026 is unchanged: Human and Bot resume there, an Agent may not.
    for token in [&ctx.human, &ctx.bot] {
        let (status, body) = ctx.respawn(token, None, None, &foreign, json!({})).await?;
        assert_eq!(status, 400, "{body}");
    }
    let (status, body) = ctx
        .post(
            &main.token,
            &format!("/v1/instances/{owned}/resume"),
            json!({}),
        )
        .await?;
    assert_refused(status, &body, 403, "agent-route");
    let (status, body) = ctx
        .post(
            &ctx.human,
            &format!("/v1/instances/{foreign}/resume"),
            json!({}),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let operator = body["instance"]["instanceId"].as_str().context("id")?;
    let (record, _) = ctx.record(operator).await?;
    assert_eq!(record["parentInstanceId"], foreign.as_str(), "D-026's edge");
    Ok(())
}

/// A resume never runs an instance beyond what OA1 allows the caller now:
/// once the owner turns Main's own control back on, a child Main created
/// control-off is refused control-off, an inherited mode inherits Main's
/// mode now, and nothing is launched for a refusal.
#[tokio::test]
async fn resume_never_escalates_beyond_the_creator() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main = ctx
        .seat_with_mode(&ctx.project, "bypassPermissions")
        .await?;
    let inherited = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    let named = ctx
        .ok(
            &main.token,
            "/v1/instances",
            json!({ "projectId": ctx.project, "hostId": ctx.host_a, "prompt": "work",
                    "driver": "claude-pty", "model": "claude-sonnet-5-5",
                    "permissionMode": "bypassPermissions" }),
        )
        .await?["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_string();
    for (id, session) in [(&inherited, "session-i"), (&named, "session-n")] {
        let (_, spec) = ctx.record(id).await?;
        assert_eq!(spec["permissionMode"], "bypassPermissions", "{spec}");
        ctx.ended_session(&ctx.host_a, id, session).await?;
    }
    // The owner turns Main's own control back on.
    let (status, body) = ctx
        .configure(
            &ctx.human,
            None,
            &main.id,
            json!({ "permissionMode": "acceptEdits" }),
        )
        .await?;
    assert_eq!(status, 200, "{body}");

    for (target, request) in [
        (&named, json!({})),
        (&named, json!({ "permissionMode": "bypassPermissions" })),
        (&inherited, json!({ "permissionMode": "dontAsk" })),
    ] {
        let (status, body) = ctx
            .respawn(&main.token, None, None, target, request)
            .await?;
        assert_refused(status, &body, 403, "beyond-creator");
    }
    assert!(
        ctx.frames_a.of("instance.resume").is_empty(),
        "a refusal launches nothing"
    );

    let (status, body) = ctx
        .respawn(&main.token, None, None, &inherited, json!({}))
        .await?;
    assert_eq!(status, 200, "{body}");
    let (_, spec) = ctx
        .record(body["instance"]["instanceId"].as_str().context("id")?)
        .await?;
    assert_eq!(spec["permissionMode"], "acceptEdits", "{spec}");
    assert_eq!(spec["permissionModeRequested"], Value::Null);
    assert_eq!(spec["creatorPermissionMode"], "acceptEdits");
    let (status, body) = ctx
        .respawn(
            &main.token,
            None,
            None,
            &named,
            json!({ "permissionMode": "auto" }),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (_, spec) = ctx
        .record(body["instance"]["instanceId"].as_str().context("id")?)
        .await?;
    assert_eq!(spec["permissionMode"], "auto", "{spec}");
    assert_eq!(ctx.frames_a.of("instance.resume").len(), 2);
    Ok(())
}

/// Another host, or a shell-driver carrier, asks the owner once (D-017)
/// before anything is launched; a denial resumes nothing, and an approved
/// retry resumes on the instance's own host.
#[tokio::test]
async fn a_cross_host_resume_asks_the_owner_and_a_denial_resumes_nothing() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main = ctx.seat(&ctx.project).await?;
    let cross = ctx
        .create_approved(
            &main.token,
            json!({ "projectId": ctx.project, "hostId": ctx.host_b, "prompt": "work",
                    "driver": "claude-pty", "model": "claude-sonnet-5-5" }),
        )
        .await?;
    ctx.ended_session(&ctx.host_b, &cross, "session-b").await?;

    let (status, asked) = ctx
        .respawn(&main.token, None, None, &cross, json!({}))
        .await?;
    assert_refused(status, &asked, 409, "approval-required");
    let card = asked["interactionId"].as_str().context("card")?.to_string();
    ctx.decide(&card, "deny").await?;
    let (status, body) = ctx
        .respawn(&main.token, None, Some(&card), &cross, json!({}))
        .await?;
    assert_refused(status, &body, 403, "approval-invalid");
    assert!(ctx.frames_b.of("instance.resume").is_empty(), "denied");
    let resumed_any = ctx.get(&ctx.human, "/v1/instances").await?.1["items"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["resumedFrom"] == cross.as_str());
    assert!(!resumed_any, "a denial resumes nothing");

    let (status, asked) = ctx
        .respawn(&main.token, None, None, &cross, json!({}))
        .await?;
    assert_refused(status, &asked, 409, "approval-required");
    let card = asked["interactionId"].as_str().context("card")?.to_string();
    ctx.decide(&card, "allow-once").await?;
    let (status, body) = ctx
        .respawn(&main.token, None, Some(&card), &cross, json!({}))
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["hostId"], ctx.host_b.as_str());
    assert_eq!(ctx.frames_b.of("instance.resume").len(), 1);

    // A shell-driver carrier asks too, on the seat's own host.
    let shell = ctx
        .create_approved(
            &main.token,
            json!({ "projectId": ctx.project, "hostId": ctx.host_a, "prompt": "work",
                    "driver": "shell-pty", "model": "claude-sonnet-5-5" }),
        )
        .await?;
    ctx.ended_session(&ctx.host_a, &shell, "session-s").await?;
    let (status, asked) = ctx
        .respawn(&main.token, None, None, &shell, json!({}))
        .await?;
    assert_refused(status, &asked, 409, "approval-required");
    assert!(ctx.frames_a.of("instance.resume").is_empty());
    Ok(())
}

// ── ap-attachments ─────────────────────────────────────────────────────────

impl Ctx {
    async fn upload(
        &self,
        token: &str,
        narrow: Option<&str>,
        instance: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .post(format!(
                "http://{}/v1/objects?instanceId={instance}&name=notes.txt",
                self.hub.addr
            ))
            .bearer_auth(token)
            .header("content-type", media_type)
            .body(bytes);
        if let Some(instance) = narrow {
            request = request.header("x-remuda-instance-id", instance);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        ))
    }

    async fn download(
        &self,
        token: &str,
        narrow: Option<&str>,
        object: &str,
    ) -> Result<(u16, Vec<u8>)> {
        let mut request = self
            .http
            .get(format!("http://{}/v1/objects/{object}", self.hub.addr))
            .bearer_auth(token);
        if let Some(instance) = narrow {
            request = request.header("x-remuda-instance-id", instance);
        }
        let response = request.send().await?;
        Ok((response.status().as_u16(), response.bytes().await?.to_vec()))
    }

    async fn staged(&self, token: &str, instance: &str, text: &str) -> Result<String> {
        let (status, body) = self
            .upload(
                token,
                None,
                instance,
                "text/plain",
                text.as_bytes().to_vec(),
            )
            .await?;
        anyhow::ensure!(status == 200, "upload {status}: {body}");
        Ok(body["objectId"].as_str().context("objectId")?.to_string())
    }

    fn attached_send(object: &str) -> Value {
        json!({ "operation": "instance.send", "payload": {
            "input": { "type": "prompt", "mode": "new-turn",
                       "blocks": [{ "type": "text", "text": "see [File #1]" }] },
            "attachments": [{ "objectId": object, "index": 1 }] } })
    }
}

/// An Agent (and a Human narrowed to it) stages attachments only for
/// instances it owns, within the same limits as anyone, attaches them only
/// to sends to those instances, and reads only owned instances' attachments;
/// another session's object is the same 404 as a missing one. Humans and
/// Bots are unchanged, and the D-028 in-session routes keep their own pin.
#[tokio::test]
async fn attachment_authority_matrix() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main = ctx.seat(&ctx.project).await?;
    let other = ctx.seat(&ctx.elsewhere).await?;
    let owned = ctx.child(&main.token, &ctx.project, &ctx.host_a).await?;
    let foreign = ctx.child(&ctx.human, &ctx.project, &ctx.host_a).await?;
    let outside = ctx.child(&other.token, &ctx.elsewhere, &ctx.host_a).await?;
    let foreign_object = ctx
        .staged(&ctx.human, &foreign, "the owner's notes")
        .await?;
    let outside_object = ctx
        .staged(&other.token, &outside, "another seat's notes")
        .await?;

    for (token, narrow) in [
        (main.token.as_str(), None),
        (ctx.human.as_str(), Some(main.id.as_str())),
    ] {
        // Upload for an owned instance; the read gives the same bytes back.
        let text = format!("notes via {narrow:?}");
        let (status, staged) = ctx
            .upload(
                token,
                narrow,
                &owned,
                "text/plain",
                text.clone().into_bytes(),
            )
            .await?;
        assert_eq!(status, 200, "{staged}");
        assert_eq!(staged["instanceId"], owned.as_str());
        assert_eq!(staged["mediaType"], "text/plain");
        let object = staged["objectId"].as_str().context("objectId")?;
        let (status, bytes) = ctx.download(token, narrow, object).await?;
        assert_eq!(status, 200);
        assert_eq!(bytes, text.as_bytes());
        // Attach it to a send to that instance.
        let (status, sent) = ctx
            .call(
                reqwest::Method::POST,
                token,
                narrow,
                &format!("/v1/instances/{owned}/commands"),
                Some(Ctx::attached_send(object)),
            )
            .await?;
        assert_eq!(status, 200, "{sent}");
        assert_eq!(
            sent["command"]["payload"]["attachments"][0]["name"],
            "notes.txt"
        );

        // Never for, from, or to what it does not own; an unknown id is the
        // same refusal.
        for target in [&foreign, &outside, &"ins_unknown".to_string()] {
            let (status, body) = ctx
                .upload(token, narrow, target, "text/plain", b"x".to_vec())
                .await?;
            assert_refused(status, &body, 403, "not-owner");
        }
        let (_, missing) = ctx.download(token, narrow, "obj_missing").await?;
        for theirs in [&foreign_object, &outside_object] {
            let (status, body) = ctx.download(token, narrow, theirs).await?;
            assert_eq!(status, 404, "{theirs}");
            assert_eq!(body, missing, "no existence leak");
        }
        let (status, _) = ctx
            .call(
                reqwest::Method::POST,
                token,
                narrow,
                &format!("/v1/instances/{owned}/commands"),
                Some(Ctx::attached_send(&foreign_object)),
            )
            .await?;
        assert_eq!(status, 403, "another session's object");
        let (status, body) = ctx
            .call(
                reqwest::Method::POST,
                token,
                narrow,
                &format!("/v1/instances/{foreign}/commands"),
                Some(Ctx::attached_send(&foreign_object)),
            )
            .await?;
        assert_refused(status, &body, 403, "not-owner");

        // The same limits as any caller: size and type.
        let (status, body) = ctx
            .upload(
                token,
                narrow,
                &owned,
                "text/plain",
                vec![b'x'; ATTACHMENT_MAX + 1],
            )
            .await?;
        assert_eq!(status, 413, "{body}");
        assert_eq!(body["code"], "RESOURCE_LIMIT");
        let (status, _) = ctx
            .upload(token, narrow, &owned, "text/ht ml", b"x".to_vec())
            .await?;
        assert_eq!(status, 400);
        let (status, _) = ctx
            .upload(
                token,
                narrow,
                &owned,
                "image/gif",
                b"\x89PNG\r\n\x1a\nrest".to_vec(),
            )
            .await?;
        assert_eq!(status, 400, "a declared type must agree with the bytes");
        // D-028's in-session reads stay pinned to the caller's own session.
        let (status, _) = ctx
            .call(
                reqwest::Method::GET,
                token,
                narrow,
                &format!("/v1/attachments?instanceId={owned}"),
                None,
            )
            .await?;
        assert_eq!(status, 403);
    }

    // Humans and Bots are unchanged.
    for token in [&ctx.human, &ctx.bot] {
        let object = ctx.staged(token, &outside, "operator notes").await?;
        for theirs in [&object, &foreign_object, &outside_object] {
            assert_eq!(ctx.download(token, None, theirs).await?.0, 200);
        }
    }
    Ok(())
}
