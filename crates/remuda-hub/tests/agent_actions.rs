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
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
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
