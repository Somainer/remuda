//! cb-usage-resume (D-060 R3, main-agent.md §11.6): wait for a subscription
//! usage-limit reset, write the reset fact, and optionally continue the
//! instance once, against the in-process Hub and one fake Node.
//!
//! Journal appends go through the store as a Node's would; the owner's and
//! the seat's input goes over HTTP. Time is injected: the wait's jitter by
//! `test_set_resume_jitter`, the fire time by `test_inbox_tick(now)`. "No
//! continuation" is a state (the fake Node's frames after a settle), never a
//! timeout.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// The jitter every test injects, in milliseconds.
const JITTER_MS: u64 = 1_234;

// ─── fake Node ──────────────────────────────────────────────────────────────

/// Every Hub→Node request frame, in arrival order.
#[derive(Clone, Default)]
struct Seen {
    frames: Arc<Mutex<Vec<Value>>>,
}

impl Seen {
    fn sends_to(&self, instance: &str) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame["method"] == "instance.send")
            .filter(|frame| frame["params"]["instanceId"] == instance)
            .map(|frame| frame["params"].clone())
            .collect()
    }

    fn hub_sends_to(&self, instance: &str) -> Vec<Value> {
        self.sends_to(instance)
            .into_iter()
            .filter(|send| send["origin"] == "hub")
            .collect()
    }
}

struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    node_token: Option<String>,
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
    match tokio::time::timeout(Duration::from_secs(30), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
    }
}

/// A Node that advertises the fence capability, records every Hub request
/// and accepts it.
async fn connect_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace: &str,
    bearer: &str,
    seen: Seen,
) -> Result<FakeNode> {
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
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
                "capabilities": { "lineageFence": true },
                "label": format!("fake-{host_id}"),
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "maxInstances": 32,
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
    anyhow::ensure!(hello.get("result").is_some(), "hello {hello}");
    let node_token = hello["result"]["nodeToken"].as_str().map(str::to_owned);
    let task = tokio::spawn(async move {
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
            seen.frames.lock().unwrap().push(frame.clone());
            let response = json!({ "jsonrpc": "2.0", "id": id, "result": { "accepted": true } });
            if node
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(FakeNode { task, node_token })
}

// ─── harness ────────────────────────────────────────────────────────────────

struct Ctx {
    dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
    seen: Seen,
    node: Option<FakeNode>,
    node_token: String,
}

fn test_config(dir: &std::path::Path) -> HubConfig {
    let mut config = HubConfig::for_test(dir.join("hub"));
    config.command_accept_timeout_ms = 500;
    config
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(test_config(dir.path())).await?;
        hub.test_set_resume_jitter(Some(JITTER_MS));
        let human = hub.mint_device_token("resume-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?;
        let seen = Seen::default();
        let node = connect_node(&hub, &host, &workspace, &enroll, seen.clone()).await?;
        let node_token = node.node_token.clone().context("node token")?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            seen,
            node: Some(node),
            node_token,
        })
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.dir.path().join("hub").join("hub.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
    }

    async fn settle(&self) -> Result<()> {
        self.hub.test_settle_inbox().await
    }

    /// Drop the Node's link, stop the Hub, start a new one on the same data
    /// directory, and reconnect the Node to it.
    async fn restart(mut self) -> Result<Ctx> {
        self.node.take();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let state: String = self.db()?.query_row(
                "SELECT state FROM hosts WHERE id = ?1",
                [&self.host],
                |row| row.get(0),
            )?;
            if state != "online" {
                break;
            }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "host never offline");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let Ctx {
            dir,
            hub,
            human,
            host,
            workspace,
            seen,
            node_token,
            ..
        } = self;
        hub.shutdown().await;
        let hub = spawn(test_config(dir.path())).await?;
        hub.test_set_resume_jitter(Some(JITTER_MS));
        let node = connect_node(&hub, &host, &workspace, &node_token, seen.clone()).await?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            seen,
            node: Some(node),
            node_token,
        })
    }

    async fn request(
        &self,
        method: reqwest::Method,
        token: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base()))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> Result<Value> {
        let (status, body) = self
            .request(reqwest::Method::POST, token, path, Some(body))
            .await?;
        anyhow::ensure!((200..300).contains(&status), "POST {path} {status}: {body}");
        Ok(body)
    }

    async fn project(&self, name: &str) -> Result<String> {
        let created = self
            .post(
                &self.human,
                "/v1/projects",
                json!({ "name": name, "members": [
                    {"hostId": self.host, "workspaceId": self.workspace, "role": "primary"}] }),
            )
            .await?;
        Ok(created["id"].as_str().context("project id")?.to_string())
    }

    async fn agent_token(&self, instance_id: &str) -> Result<String> {
        let value = self
            .post(
                &self.human,
                &format!("/v1/instances/{instance_id}/mcp-token"),
                json!({}),
            )
            .await?;
        Ok(value["token"].as_str().context("mcp token")?.to_string())
    }

    /// A human-seated project coordinator (a continuity lineage), running
    /// and idle.
    async fn seat(&self, project: &str) -> Result<(String, String)> {
        let created = self
            .post(
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
        let token = self.agent_token(&id).await?;
        self.journal(&id, entity("running")).await?;
        self.journal(&id, activity("idle")).await?;
        Ok((id, token))
    }

    /// A worker the seat behind `token` created, running and idle; the
    /// seat sets its `autoContinue` at create (§11.6).
    async fn worker(&self, token: &str, project: &str, auto_continue: bool) -> Result<String> {
        let created = self
            .post(
                token,
                "/v1/instances",
                json!({ "role": "worker", "projectId": project, "prompt": "work",
                        "driver": "claude-pty", "autoContinue": auto_continue }),
            )
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string();
        assert_eq!(
            created["instance"]["autoContinue"], auto_continue,
            "{created}"
        );
        self.journal(&id, entity("running")).await?;
        Ok(id)
    }

    async fn journal(&self, instance_id: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        Ok(())
    }

    /// An input to `instance` over HTTP from `token`'s caller; returns its
    /// commandId.
    async fn send(&self, token: &str, instance: &str, text: &str) -> Result<String> {
        let body = self
            .post(
                token,
                &format!("/v1/instances/{instance}/commands"),
                json!({ "operation": "instance.send",
                        "payload": { "input": { "type": "prompt", "text": text } } }),
            )
            .await?;
        Ok(body["command"]["commandId"]
            .as_str()
            .or_else(|| body["commandId"].as_str())
            .context(format!("commandId in {body}"))?
            .to_owned())
    }

    /// Settle a command the way a Node's journal does.
    async fn settle_command(&self, instance: &str, command: &str) -> Result<()> {
        self.journal(
            instance,
            json!({ "kind": "lifecycle", "payload": {
                "type": "entity", "entityType": "command", "entityId": command,
                "state": "settled",
                "entity": { "commandId": command, "state": "settled",
                            "settlement": { "state": "known", "value": { "outcome": "completed" } } } } }),
        )
        .await
    }

    /// One root turn of `worker` for `command` that hits a subscription
    /// limit: the harness's structured rejection (when `resets` is given),
    /// optional real output, then the limit notice and a failed result.
    async fn limit_turn(
        &self,
        worker: &str,
        command: &str,
        resets: Option<i64>,
        output: bool,
        notice: &str,
    ) -> Result<()> {
        self.journal(worker, turn_started(command)).await?;
        if output {
            self.journal(worker, tool_call(&format!("tc_{command}")))
                .await?;
            self.journal(
                worker,
                assistant(&format!("msg_out_{command}"), "half way there"),
            )
            .await?;
        }
        if let Some(resets) = resets {
            self.journal(worker, rate_limit_event("rejected", resets))
                .await?;
        }
        self.journal(worker, assistant(&format!("msg_{command}"), notice))
            .await?;
        self.journal(worker, turn_result("error", command, Some(notice)))
            .await?;
        Ok(())
    }

    /// One root turn of `worker` for `command` that does work and ends.
    async fn good_turn(&self, worker: &str, command: &str) -> Result<()> {
        self.journal(worker, turn_started(command)).await?;
        self.journal(worker, tool_call(&format!("tc_{command}")))
            .await?;
        self.journal(worker, assistant(&format!("msg_{command}"), "all green"))
            .await?;
        self.journal(worker, turn_result("turn_done", command, None))
            .await?;
        Ok(())
    }

    /// The wait rows of `instance`, oldest first.
    fn waits(&self, instance: &str) -> Result<Vec<WaitRow>> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT state, reason, fire_at, resets_at, source, window_id, attempts, command_id,
                    initiator_instance_id, initiator_generation
               FROM resume_on_reset WHERE instance_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([instance], |row| {
                Ok(WaitRow {
                    state: row.get(0)?,
                    reason: row.get(1)?,
                    fire_at: row.get(2)?,
                    resets_at: row.get(3)?,
                    source: row.get(4)?,
                    window: row.get(5)?,
                    attempts: row.get(6)?,
                    command_id: row.get(7)?,
                    initiator_instance: row.get(8)?,
                    initiator_generation: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The inbox rows of `kind` the seat lineage holds.
    async fn facts(&self, lineage: &str, kind: &str) -> Result<Vec<Value>> {
        let (status, body) = self
            .request(
                reqwest::Method::GET,
                &self.human,
                &format!("/v1/lineages/{lineage}/inbox?limit=200"),
                None,
            )
            .await?;
        anyhow::ensure!(status == 200, "GET inbox {lineage}: {status} {body}");
        Ok(body["items"]
            .as_array()
            .context("items")?
            .iter()
            .filter(|fact| fact["kind"] == kind)
            .cloned()
            .collect())
    }

    /// One inbox tick at `now`, then the delivery driver.
    async fn tick(&self, now: &str) -> Result<()> {
        self.hub.test_inbox_tick(now).await?;
        self.settle().await
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct WaitRow {
    state: String,
    reason: Option<String>,
    fire_at: String,
    resets_at: String,
    source: String,
    window: String,
    attempts: i64,
    command_id: Option<String>,
    initiator_instance: Option<String>,
    initiator_generation: Option<i64>,
}

// ─── events and time ────────────────────────────────────────────────────────

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn stamp(at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.millisecond()
    )
}

fn parse(raw: &str) -> OffsetDateTime {
    OffsetDateTime::parse(raw, &Rfc3339).expect("rfc3339")
}

/// A reset time `secs` from now, whole seconds.
fn resets_in(secs: i64) -> i64 {
    now().unix_timestamp() + secs
}

fn entity(state: &str) -> Value {
    json!({ "kind": "lifecycle",
            "payload": { "type": "entity", "entityType": "instance", "state": state } })
}

fn activity(value: &str) -> Value {
    json!({ "kind": "lifecycle", "payload": {
        "type": "native", "topic": "session", "nativeName": "session",
        "nativeId": { "state": "known", "value": "session-seat" },
        "status": { "state": "known", "value": value },
        "relatedIds": {}, "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn turn_started(command: &str) -> Value {
    json!({ "kind": "lifecycle", "payload": {
        "type": "native", "topic": "turn", "nativeName": "turn_started",
        "nativeId": { "state": "known", "value": "session-1" },
        "status": { "state": "known", "value": "working" },
        "relatedIds": { "nativeClientMessageId": command },
        "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn turn_result(status: &str, command: &str, error: Option<&str>) -> Value {
    let mut related = serde_json::Map::new();
    related.insert("completedTurn".into(), json!(command));
    related.insert("settledRootTurn".into(), json!("true"));
    if let Some(text) = error {
        related.insert("lastError".into(), json!(text));
    }
    json!({ "kind": "lifecycle", "payload": {
        "type": "native", "topic": "turn", "nativeName": "result",
        "nativeId": { "state": "known", "value": "session-1" },
        "status": { "state": "known", "value": status },
        "relatedIds": related, "dataRef": null, "severity": "info", "affectsCompletion": true } })
}

/// The carrier's `rate_limit_event` diagnostic, as the claude-sdk and
/// claude-print mappers write it (cb-usage-resume item 1).
fn rate_limit_event(status: &str, resets_at: i64) -> Value {
    json!({ "kind": "lifecycle", "payload": {
        "type": "native", "topic": "diagnostic", "nativeName": "rate_limit_event",
        "nativeId": { "state": "known", "value": "session-1" },
        "status": { "state": "known", "value": status },
        "relatedIds": { "rateLimitStatus": status, "rateLimitResetsAt": resets_at.to_string(),
                        "rateLimitType": "five_hour", "rateLimitOverageStatus": "rejected",
                        "rateLimitUsingOverage": "false" },
        "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

/// The carrier's session start, with the harness's credential source.
fn session_started(api_key_source: &str) -> Value {
    json!({ "kind": "lifecycle", "payload": {
        "type": "native", "topic": "session", "nativeName": "session",
        "nativeId": { "state": "known", "value": "session-1" },
        "status": { "state": "known", "value": "started" },
        "relatedIds": { "model": "claude-sonnet-5-5", "apiKeySource": api_key_source },
        "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn assistant(message: &str, text: &str) -> Value {
    json!({ "kind": "message", "payload": {
        "operation": "open", "messageId": message, "role": "assistant", "phase": "final",
        "parentToolCallId": null, "blocks": [{ "type": "text", "text": text }] } })
}

fn tool_call(id: &str) -> Value {
    json!({ "kind": "tool_call", "payload": {
        "toolCallId": id, "toolName": "Bash", "input": { "command": "true" } } })
}

const NOTICE: &str = "You've hit your limit · resets 3pm (Asia/Shanghai)";

// ─── tests ──────────────────────────────────────────────────────────────────

/// A subscription 429 with a known `resetsAt`: exactly one limit fact (the
/// failed turn's `turn-failed`, carrying the wait), one wait row whose fire
/// time is `resetsAt` + 60 s + the injected jitter, and exactly one reset
/// fact once that time comes, never before and never twice. With
/// `autoContinue` off nothing is sent to the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscription_limit_gives_one_limit_fact_then_one_reset_fact_at_the_fire_time()
-> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-basic").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, false).await?;
    let resets = resets_in(3_600);
    ctx.limit_turn(&worker, "cmd_w_1", Some(resets), true, NOTICE)
        .await?;
    ctx.settle().await?;

    let limits = ctx.facts(&seat, "turn-failed").await?;
    assert_eq!(limits.len(), 1, "one limit fact: {limits:#?}");
    let limit = &limits[0]["payload"]["usageLimit"];
    assert_eq!(limit["class"], "subscription", "{limit}");
    assert_eq!(limit["source"], "structured", "{limit}");
    assert_eq!(limit["window"], "five_hour", "{limit}");
    assert_eq!(limit["waits"], true, "{limit}");
    assert_eq!(limit["autoContinue"], false, "{limit}");
    let resets_at = OffsetDateTime::from_unix_timestamp(resets)?;
    assert_eq!(limit["resetsAt"], stamp(resets_at));
    let fire_at =
        resets_at + time::Duration::seconds(60) + time::Duration::milliseconds(JITTER_MS as i64);
    assert_eq!(limit["fireAt"], stamp(fire_at));

    let waits = ctx.waits(&worker)?;
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0].state, "waiting");
    assert_eq!(waits[0].fire_at, stamp(fire_at));
    assert_eq!(waits[0].source, "structured");
    assert_eq!(waits[0].window, "five_hour");
    assert!(ctx.facts(&seat, "supply-reset").await?.is_empty());

    // A tick a second early fires nothing.
    ctx.tick(&stamp(fire_at - time::Duration::seconds(1)))
        .await?;
    assert!(ctx.facts(&seat, "supply-reset").await?.is_empty());
    assert_eq!(ctx.waits(&worker)?[0].state, "waiting");

    // At the fire time: one reset fact, the row claimed once.
    ctx.tick(&stamp(fire_at)).await?;
    ctx.tick(&stamp(fire_at + time::Duration::seconds(30)))
        .await?;
    let resets_facts = ctx.facts(&seat, "supply-reset").await?;
    assert_eq!(resets_facts.len(), 1, "one reset fact: {resets_facts:#?}");
    let payload = &resets_facts[0]["payload"];
    assert_eq!(payload["window"], "five_hour");
    assert_eq!(payload["resetsAt"], stamp(resets_at));
    assert_eq!(payload["autoContinue"], "off");
    assert_eq!(resets_facts[0]["subjectInstanceId"], worker.as_str());
    let waits = ctx.waits(&worker)?;
    assert_eq!(waits[0].state, "done");
    assert_eq!(waits[0].reason.as_deref(), Some("auto-continue-off"));
    assert!(
        ctx.seen.hub_sends_to(&worker).is_empty(),
        "autoContinue off: nothing is sent to the worker"
    );
    Ok(())
}

/// Only subscription windows wait. An API-key or gateway 429 makes no row,
/// even with a structured rejection and a retry time in its text; neither
/// does a per-minute API rate limit on the host's own login. Each still has
/// its usual `turn-failed` fact, and no reset fact ever follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_or_gateway_429_makes_no_wait_row() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-apikey").await?;
    let (seat, token) = ctx.seat(&project).await?;

    // A gateway profile's worker.
    let provider = ctx
        .post(
            &ctx.human,
            "/v1/providers",
            json!({ "name": "relay", "kind": "gateway", "baseUrl": "https://gateway.example",
                    "authToken": "test-token-not-real" }),
        )
        .await?;
    let profile = provider["id"]
        .as_str()
        .or_else(|| provider["provider"]["id"].as_str())
        .context(format!("provider id in {provider}"))?
        .to_owned();
    let gateway_worker = ctx.worker(&token, &project, true).await?;
    ctx.db()?.execute(
        "UPDATE instances SET spec_json = json_set(spec_json, '$.providerProfileId', ?2)
          WHERE id = ?1",
        rusqlite::params![gateway_worker, profile],
    )?;
    let retry = "Request rejected (429) · rate limited, retry at 2026-11-03T10:00:00Z";
    ctx.limit_turn(
        &gateway_worker,
        "cmd_g_1",
        Some(resets_in(600)),
        false,
        retry,
    )
    .await?;

    // The host's own login, hitting a per-minute API rate limit.
    let host_worker = ctx.worker(&token, &project, true).await?;
    let api = "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\
               \"message\":\"Number of request tokens has exceeded your per-minute rate limit\"}} \
               · retry after 2026-11-03T10:00:00Z";
    ctx.limit_turn(&host_worker, "cmd_h_1", None, false, api)
        .await?;
    ctx.settle().await?;

    assert!(ctx.waits(&gateway_worker)?.is_empty(), "no wait row");
    assert!(ctx.waits(&host_worker)?.is_empty(), "no wait row");
    let failed = ctx.facts(&seat, "turn-failed").await?;
    assert_eq!(failed.len(), 2, "the usual error facts: {failed:#?}");
    let class_of = |worker: &str| {
        failed
            .iter()
            .find(|fact| fact["subjectInstanceId"] == worker)
            .map(|fact| fact["payload"]["usageLimit"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(class_of(&gateway_worker)["class"], "api-key-or-gateway");
    assert_eq!(class_of(&gateway_worker)["waits"], false);
    assert_eq!(class_of(&host_worker)["class"], "api-rate-limit");
    assert_eq!(class_of(&host_worker)["waits"], false);

    // No wait ever fires: no reset fact of a wait (one carries its fire
    // time; a gateway's cooling park keeps its own cb-inbox-facts fact), and
    // nothing is sent.
    ctx.tick(&stamp(now() + time::Duration::days(2))).await?;
    assert!(
        ctx.facts(&seat, "supply-reset")
            .await?
            .iter()
            .all(|fact| fact["payload"]["fireAt"].is_null()),
        "no wait fired"
    );
    assert!(ctx.seen.hub_sends_to(&gateway_worker).is_empty());
    assert!(ctx.seen.hub_sends_to(&host_worker).is_empty());
    Ok(())
}

/// A reset more than 8 days away makes no row, only the fact; a reset time
/// from a notice that states no zone makes none either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reset_beyond_eight_days_gives_the_fact_only() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-far").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let far = ctx.worker(&token, &project, true).await?;
    ctx.limit_turn(&far, "cmd_f_1", Some(resets_in(9 * 86_400)), true, NOTICE)
        .await?;
    let unzoned = ctx.worker(&token, &project, true).await?;
    ctx.limit_turn(
        &unzoned,
        "cmd_u_1",
        None,
        true,
        "You've hit your limit · resets 3pm",
    )
    .await?;
    ctx.settle().await?;

    assert!(ctx.waits(&far)?.is_empty(), "no row beyond 8 days");
    assert!(ctx.waits(&unzoned)?.is_empty(), "no row without a zone");
    let failed = ctx.facts(&seat, "turn-failed").await?;
    let detail = |worker: &str| {
        failed
            .iter()
            .find(|fact| fact["subjectInstanceId"] == worker)
            .map(|fact| fact["payload"]["usageLimit"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(detail(&far)["class"], "subscription");
    assert_eq!(detail(&far)["waits"], false);
    assert_eq!(detail(&far)["reason"], "beyond-8-days");
    assert_eq!(detail(&unzoned)["waits"], false);
    assert_eq!(detail(&unzoned)["reason"], "no-reset-time");

    ctx.tick(&stamp(now() + time::Duration::days(10))).await?;
    assert!(ctx.facts(&seat, "supply-reset").await?.is_empty());
    assert!(ctx.seen.hub_sends_to(&far).is_empty());
    Ok(())
}

/// The zoned time in the notice is the third source: used when the turn had
/// no structured event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zoned_time_in_the_notice_is_the_reset_time_without_a_structured_event() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-text").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, false).await?;
    let reset = now() + time::Duration::hours(2);
    let hhmm = format!("{:02}:{:02}", reset.hour(), reset.minute());
    let notice = format!("You've hit your session limit · resets {hhmm} (UTC)");
    ctx.limit_turn(&worker, "cmd_t_1", None, true, &notice)
        .await?;
    ctx.settle().await?;
    let waits = ctx.waits(&worker)?;
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0].source, "error-text");
    let expected = reset.replace_second(0)?.replace_millisecond(0)?;
    assert_eq!(parse(&waits[0].resets_at), expected);
    assert_eq!(
        ctx.facts(&seat, "turn-failed").await?[0]["payload"]["usageLimit"]["source"],
        "error-text"
    );
    Ok(())
}

/// autoContinue continues once per reset: after the reset fact, one
/// `instance.send` with origin `hub`, a persisted commandId and the seat
/// lineage's Hub-internal initiator, whose root turn is origin `hub`. A
/// turn that produced output gets `continue`; one that produced none gets
/// its own input again. Three continuations without substantive progress
/// are allowed; the 4th reset is capped (fact only); progress lifts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_continue_continues_once_and_the_fourth_is_capped() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-cap").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, true).await?;
    let original = ctx.send(&token, &worker, "run the migration").await?;
    ctx.settle().await?;
    ctx.settle_command(&worker, &original).await?;

    // Turn 1 does real work, then hits the limit.
    ctx.limit_turn(&worker, &original, Some(resets_in(600)), true, NOTICE)
        .await?;
    let mut continuations: Vec<String> = Vec::new();
    for round in 1..=4 {
        ctx.settle().await?;
        let waits = ctx.waits(&worker)?;
        assert_eq!(waits.len(), round, "{waits:?}");
        let fire_at = waits[round - 1].fire_at.clone();
        ctx.tick(&fire_at).await?;
        let resets_facts = ctx.facts(&seat, "supply-reset").await?;
        assert_eq!(
            resets_facts.len(),
            round,
            "a reset fact per wait, always first"
        );
        let sends = ctx.seen.hub_sends_to(&worker);
        if round == 4 {
            assert_eq!(sends.len(), 3, "the 4th is capped: {sends:#?}");
            assert_eq!(resets_facts[3]["payload"]["autoContinue"], "capped");
            let waits = ctx.waits(&worker)?;
            assert_eq!(waits[3].state, "capped");
            assert!(waits[3].command_id.is_none());
            break;
        }
        assert_eq!(sends.len(), round, "exactly one continuation: {sends:#?}");
        let send = &sends[round - 1];
        let command = send["commandId"].as_str().context("commandId")?.to_owned();
        assert!(!continuations.contains(&command), "a new command each time");
        assert_ne!(command, original, "never a replay of the failed turn's id");
        assert_eq!(send["origin"], "hub");
        assert_eq!(send["input"]["origin"], "hub");
        assert_eq!(send["initiator"]["lineageId"], seat.as_str());
        assert_eq!(send["initiator"]["instanceId"], seat.as_str());
        assert_eq!(send["initiator"]["generation"], 1);
        let waits = ctx.waits(&worker)?;
        assert_eq!(waits[round - 1].state, "continued");
        assert_eq!(
            waits[round - 1].command_id.as_deref(),
            Some(command.as_str())
        );
        assert_eq!(waits[round - 1].attempts, round as i64);
        // The reset fact was written before the command row.
        let fact_at = resets_facts[round - 1]["createdAt"]
            .as_str()
            .unwrap()
            .to_owned();
        let command_at: String = ctx.db()?.query_row(
            "SELECT created_at FROM commands WHERE id = ?1",
            [&command],
            |row| row.get(0),
        )?;
        assert!(
            fact_at <= command_at,
            "fact {fact_at} before command {command_at}"
        );
        if round == 1 {
            assert_eq!(
                send["input"]["text"], "continue",
                "the failed turn had output"
            );
        } else {
            // The previous continuation's turn produced nothing: its own
            // input again.
            assert_eq!(send["input"]["text"], "continue");
        }
        continuations.push(command.clone());

        // The continuation runs a turn that is origin hub, and hits the
        // limit again at once (no substantive progress).
        ctx.settle_command(&worker, &command).await?;
        ctx.journal(&worker, turn_started(&command)).await?;
        let root = ctx
            .hub
            .store()
            .context("store")?
            .root_turn(worker.clone())
            .await?
            .context("root turn")?;
        assert_eq!(root.command_id, command);
        assert_eq!(root.origin, remuda_protocol::InputOrigin::Hub);
        assert_eq!(
            root.continuation,
            remuda_protocol::RootTurnContinuation::Hub
        );
        ctx.journal(&worker, rate_limit_event("rejected", resets_in(600)))
            .await?;
        ctx.journal(&worker, assistant(&format!("msg_{command}"), NOTICE))
            .await?;
        ctx.journal(&worker, turn_result("error", &command, Some(NOTICE)))
            .await?;
    }

    // Substantive progress lifts the cap: the next limit continues again.
    let next = ctx.send(&ctx.human, &worker, "carry on").await?;
    ctx.settle().await?;
    ctx.settle_command(&worker, &next).await?;
    ctx.good_turn(&worker, &next).await?;
    ctx.limit_turn(&worker, "cmd_w_after", Some(resets_in(600)), false, NOTICE)
        .await?;
    ctx.settle().await?;
    let waits = ctx.waits(&worker)?;
    let last = waits.last().context("a fifth wait")?;
    assert_eq!(last.state, "waiting");
    ctx.tick(&last.fire_at.clone()).await?;
    assert_eq!(
        ctx.seen.hub_sends_to(&worker).len(),
        4,
        "progress lifted the cap"
    );
    Ok(())
}

/// A turn that produced no output is continued with its own input again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_without_output_is_continued_with_its_original_input() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-original").await?;
    let (_seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, true).await?;
    let original = ctx.send(&token, &worker, "run the migration").await?;
    ctx.settle().await?;
    ctx.settle_command(&worker, &original).await?;
    ctx.limit_turn(&worker, &original, Some(resets_in(600)), false, NOTICE)
        .await?;
    ctx.settle().await?;
    let fire_at = ctx.waits(&worker)?[0].fire_at.clone();
    ctx.tick(&fire_at).await?;
    let sends = ctx.seen.hub_sends_to(&worker);
    assert_eq!(sends.len(), 1, "{sends:#?}");
    assert_eq!(sends[0]["input"]["text"], "run the migration");
    assert_ne!(sends[0]["commandId"], original.as_str(), "a new command id");
    Ok(())
}

/// A failed turn the create prompt started, with no output, is continued
/// with that prompt again (the create command's `initialInput`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_first_turn_is_continued_with_the_create_prompt() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-create").await?;
    let (_seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, true).await?;
    let create: String = ctx.db()?.query_row(
        "SELECT id FROM commands WHERE instance_id = ?1 AND operation = 'instance.create'",
        [&worker],
        |row| row.get(0),
    )?;
    ctx.limit_turn(&worker, &create, Some(resets_in(600)), false, NOTICE)
        .await?;
    ctx.settle().await?;
    let fire_at = ctx.waits(&worker)?[0].fire_at.clone();
    ctx.tick(&fire_at).await?;
    let sends = ctx.seen.hub_sends_to(&worker);
    assert_eq!(sends.len(), 1, "{sends:#?}");
    assert_eq!(sends[0]["input"]["text"], "work", "the create prompt again");
    assert_eq!(sends[0]["input"]["origin"], "hub");
    Ok(())
}

/// Source 2 is a full subscription window of the instance's own native
/// profile with a reported `resetsAt`. A window parked by the Hub's backoff
/// (a cooldown and no `resetsAt`) is the Hub's own estimate, never a remote
/// Node's report, so it makes no row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_window_with_a_reported_reset_is_a_source_and_a_backoff_is_not() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-window").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let reset = resets_in(900);
    let reported = ctx
        .http
        .post(format!("{}/v1/providers", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({ "name": "login-window", "kind": "native",
                       "baseUrl": "https://api.anthropic.com",
                       "supply": { "windows": [{ "id": "five_hour", "appliesTo": ["*"],
                                                "usedPercent": 100.0, "resetsAt": reset }] } }))
        .send()
        .await?;
    let status = reported.status();
    let reported = reported.json::<Value>().await?;
    anyhow::ensure!(status.is_success(), "{status} {reported}");
    let backed_off = ctx
        .http
        .post(format!("{}/v1/providers", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({ "name": "login-backoff", "kind": "native",
                       "baseUrl": "https://api.anthropic.com",
                       "supply": { "windows": [{ "id": "model", "appliesTo": ["*"],
                                                "usedPercent": 100.0,
                                                "cooldownUntil": reset }] } }))
        .send()
        .await?;
    anyhow::ensure!(backed_off.status().is_success(), "{backed_off:?}");
    let backed_off = backed_off.json::<Value>().await?;
    let id = |value: &Value| {
        value["id"]
            .as_str()
            .or_else(|| value["provider"]["id"].as_str())
            .expect("provider id")
            .to_owned()
    };
    let with_window = ctx.worker(&token, &project, true).await?;
    let with_backoff = ctx.worker(&token, &project, true).await?;
    for (worker, provider) in [
        (&with_window, id(&reported)),
        (&with_backoff, id(&backed_off)),
    ] {
        ctx.db()?.execute(
            "UPDATE instances SET spec_json = json_set(spec_json, '$.providerProfileId', ?2)
              WHERE id = ?1",
            rusqlite::params![worker, provider],
        )?;
    }
    // No structured event: the notice alone.
    ctx.limit_turn(&with_window, "cmd_v_1", None, true, NOTICE)
        .await?;
    // No zone stated either, so the backoff is the only candidate left.
    ctx.limit_turn(
        &with_backoff,
        "cmd_b_1",
        None,
        true,
        "You've hit your limit · resets 3pm",
    )
    .await?;
    ctx.settle().await?;
    let waits = ctx.waits(&with_window)?;
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0].source, "supply-window");
    assert_eq!(
        parse(&waits[0].resets_at).unix_timestamp(),
        reset,
        "the Node's reported resetsAt"
    );
    assert!(
        ctx.waits(&with_backoff)?.is_empty(),
        "a backoff is not a report"
    );
    let failed = ctx.facts(&seat, "turn-failed").await?;
    let detail = |worker: &str| {
        failed
            .iter()
            .find(|fact| fact["subjectInstanceId"] == worker)
            .map(|fact| fact["payload"]["usageLimit"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(detail(&with_window)["source"], "supply-window");
    assert_eq!(detail(&with_backoff)["waits"], false);
    Ok(())
}

/// The harness's own report decides when the instance has no profile: a
/// session whose `apiKeySource` names an API key makes no row even with a
/// structured `rejected` event, and one reporting `none` (a claude.ai login)
/// waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_session_makes_no_row_even_with_a_structured_rejection() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-keysource").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let keyed = ctx.worker(&token, &project, true).await?;
    let login = ctx.worker(&token, &project, true).await?;
    for (worker, source) in [(&keyed, "ANTHROPIC_API_KEY"), (&login, "none")] {
        ctx.journal(worker, session_started(source)).await?;
    }
    ctx.limit_turn(&keyed, "cmd_k_1", Some(resets_in(600)), false, NOTICE)
        .await?;
    ctx.limit_turn(&login, "cmd_l_1", Some(resets_in(600)), false, NOTICE)
        .await?;
    ctx.settle().await?;
    assert!(ctx.waits(&keyed)?.is_empty(), "an API key never waits");
    assert_eq!(ctx.waits(&login)?.len(), 1, "a claude.ai login waits");
    let failed = ctx.facts(&seat, "turn-failed").await?;
    let class_of = |worker: &str| {
        failed
            .iter()
            .find(|fact| fact["subjectInstanceId"] == worker)
            .map(|fact| fact["payload"]["usageLimit"]["class"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(class_of(&keyed), "api-key-or-gateway");
    assert_eq!(class_of(&login), "subscription");
    Ok(())
}

/// Any Human or Agent input cancels the wait, and a switch-model is such an
/// input: no reset fact and no continuation follow a cancelled wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn any_input_cancels_the_wait() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-cancel").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let human_input = ctx.worker(&token, &project, true).await?;
    let agent_input = ctx.worker(&token, &project, true).await?;
    let switch_model = ctx.worker(&token, &project, true).await?;
    for (n, worker) in [&human_input, &agent_input, &switch_model]
        .into_iter()
        .enumerate()
    {
        ctx.limit_turn(
            worker,
            &format!("cmd_c_{n}"),
            Some(resets_in(600)),
            true,
            NOTICE,
        )
        .await?;
    }
    ctx.settle().await?;
    for worker in [&human_input, &agent_input, &switch_model] {
        assert_eq!(ctx.waits(worker)?[0].state, "waiting");
    }

    ctx.send(&ctx.human, &human_input, "never mind, do this instead")
        .await?;
    ctx.send(&token, &agent_input, "skip that step").await?;
    ctx.post(
        &ctx.human,
        &format!("/v1/instances/{switch_model}/commands"),
        json!({ "operation": "instance.configure", "payload": { "model": "sonnet" } }),
    )
    .await?;
    for worker in [&human_input, &agent_input, &switch_model] {
        let waits = ctx.waits(worker)?;
        assert_eq!(waits[0].state, "cancelled", "{waits:?}");
        assert_eq!(waits[0].reason.as_deref(), Some("input"));
    }
    let fire_at = ctx.waits(&human_input)?[0].fire_at.clone();
    ctx.tick(&stamp(parse(&fire_at) + time::Duration::minutes(5)))
        .await?;
    assert!(ctx.facts(&seat, "supply-reset").await?.is_empty());
    for worker in [&human_input, &agent_input, &switch_model] {
        assert!(ctx.seen.hub_sends_to(worker).is_empty());
    }
    Ok(())
}

/// The wait survives a Hub restart: the new process fires it at its time,
/// once, and continues the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_survives_a_hub_restart() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-restart").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, true).await?;
    ctx.limit_turn(&worker, "cmd_r_1", Some(resets_in(600)), true, NOTICE)
        .await?;
    ctx.settle().await?;
    let before = ctx.waits(&worker)?;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, "waiting");

    let ctx = ctx.restart().await?;
    let after = ctx.waits(&worker)?;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].state, "waiting");
    assert_eq!(after[0].fire_at, before[0].fire_at);
    ctx.tick(&before[0].fire_at).await?;
    ctx.tick(&before[0].fire_at).await?;
    assert_eq!(ctx.facts(&seat, "supply-reset").await?.len(), 1);
    let sends = ctx.seen.hub_sends_to(&worker);
    assert_eq!(sends.len(), 1, "{sends:#?}");
    assert_eq!(sends[0]["input"]["text"], "continue");
    Ok(())
}

async fn pause(ctx: &Ctx, seat: &str) -> Result<()> {
    ctx.post(
        &ctx.human,
        &format!("/v1/instances/{seat}/commands"),
        json!({ "operation": "instance.close", "payload": {} }),
    )
    .await?;
    Ok(())
}

/// Resume the paused lineage; returns the successor chapter, running and
/// idle.
async fn resume(ctx: &Ctx, seat: &str) -> Result<String> {
    let resumed = ctx
        .post(
            &ctx.human,
            &format!("/v1/instances/{seat}/resume"),
            json!({}),
        )
        .await?;
    let successor = resumed["instance"]["instanceId"]
        .as_str()
        .or_else(|| resumed["instanceId"].as_str())
        .context(format!("successor in {resumed}"))?
        .to_owned();
    ctx.journal(&successor, entity("running")).await?;
    ctx.journal(&successor, activity("idle")).await?;
    Ok(successor)
}

/// F mid-wait re-attributes the row to the successor: the continuation
/// carries the successor chapter and the live generation. A continuation F
/// cancelled before its forward is formed again for the successor under a
/// new id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fence_mid_wait_re_attributes_the_row_to_the_successor() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-fence").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let waiting = ctx.worker(&token, &project, true).await?;
    let formed = ctx.worker(&token, &project, true).await?;
    ctx.limit_turn(&waiting, "cmd_fw_1", Some(resets_in(3_600)), true, NOTICE)
        .await?;
    ctx.limit_turn(&formed, "cmd_ff_1", Some(resets_in(600)), true, NOTICE)
        .await?;
    ctx.settle().await?;
    for worker in [&waiting, &formed] {
        let waits = ctx.waits(worker)?;
        assert_eq!(waits[0].initiator_instance.as_deref(), Some(seat.as_str()));
        assert_eq!(waits[0].initiator_generation, Some(1));
    }

    // `formed` fires and forms its continuation, held before the forward.
    ctx.hub.test_hold_inbox_forward(true);
    let formed_fire = ctx.waits(&formed)?[0].fire_at.clone();
    ctx.tick(&formed_fire).await?;
    let held = ctx.waits(&formed)?;
    assert_eq!(held[0].state, "continued");
    let held_command = held[0].command_id.clone().context("formed")?;
    assert!(ctx.seen.hub_sends_to(&formed).is_empty());

    // F: pause, then resume onto a successor chapter.
    pause(&ctx, &seat).await?;
    ctx.settle().await?;
    let released = ctx.waits(&formed)?;
    assert_eq!(
        released[0].state, "holding",
        "F handed it back: {released:?}"
    );
    assert!(released[0].command_id.is_none());
    let (state, reason): (String, Option<String>) = ctx.db()?.query_row(
        "SELECT state, settlement_reason FROM commands WHERE id = ?1",
        [&held_command],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("settled", Some("fenced"))
    );
    ctx.hub.test_hold_inbox_forward(false);
    // Paused: no live generation, so nothing is formed.
    ctx.tick(&formed_fire).await?;
    assert!(ctx.seen.hub_sends_to(&formed).is_empty());

    let successor = resume(&ctx, &seat).await?;
    for worker in [&waiting, &formed] {
        let waits = ctx.waits(worker)?;
        assert_eq!(
            waits[0].initiator_instance.as_deref(),
            Some(successor.as_str()),
            "re-attributed: {waits:?}"
        );
        assert_eq!(waits[0].initiator_generation, Some(2));
    }

    // The held continuation is formed again, under a new id, for the
    // successor; the waiting one fires later with the successor's initiator.
    ctx.tick(&formed_fire).await?;
    let sends = ctx.seen.hub_sends_to(&formed);
    assert_eq!(sends.len(), 1, "{sends:#?}");
    assert_ne!(sends[0]["commandId"], held_command.as_str(), "a new id");
    assert_eq!(sends[0]["initiator"]["instanceId"], successor.as_str());
    assert_eq!(sends[0]["initiator"]["lineageId"], seat.as_str());
    assert_eq!(sends[0]["initiator"]["generation"], 2);

    let waiting_fire = ctx.waits(&waiting)?[0].fire_at.clone();
    ctx.tick(&waiting_fire).await?;
    let sends = ctx.seen.hub_sends_to(&waiting);
    assert_eq!(sends.len(), 1, "{sends:#?}");
    assert_eq!(sends[0]["initiator"]["instanceId"], successor.as_str());
    assert_eq!(sends[0]["initiator"]["generation"], 2);
    Ok(())
}

/// The owner may turn autoContinue on later (`PATCH /v1/instances/{id}`);
/// an Agent may not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_may_turn_auto_continue_on_later() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("resume-patch").await?;
    let (_seat, token) = ctx.seat(&project).await?;
    let worker = ctx.worker(&token, &project, false).await?;
    let (status, _) = ctx
        .request(
            reqwest::Method::PATCH,
            &token,
            &format!("/v1/instances/{worker}"),
            Some(json!({ "autoContinue": true })),
        )
        .await?;
    assert_eq!(status, 403, "an Agent sets it only at dispatch or create");
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/instances/{worker}"),
            Some(json!({ "autoContinue": true })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["autoContinue"], true);
    ctx.limit_turn(&worker, "cmd_p_1", Some(resets_in(600)), true, NOTICE)
        .await?;
    ctx.settle().await?;
    let fire_at = ctx.waits(&worker)?[0].fire_at.clone();
    ctx.tick(&fire_at).await?;
    assert_eq!(ctx.seen.hub_sends_to(&worker).len(), 1);
    Ok(())
}
