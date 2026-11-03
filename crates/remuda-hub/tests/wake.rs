//! cb-wake-engine (D-060 §11, main-agent.md §11.1, §11.3, §11.4, §11.5):
//! durable wake-ups against the in-process Hub and one fake Node.
//!
//! Time is injected: every test holds the scheduler's own wake ticks and
//! drives `test_wake_tick(now)` at the instant it names, so "fires once" and
//! "fires nothing" are states, never timeouts. Wake-ups, journal appends and
//! the owner's commands go through the production routes and store paths; a
//! Hub restart reopens the same data directory.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const UNATTENDED: &str = "The owner is not watching. Put open questions in your reply or use \
                          `remuda owner notify`; do not ask an interactive question.";
const END_TURN: &str = "End the turn when the work is done; do not sleep-poll.";

// ─── fake Node ──────────────────────────────────────────────────────────────

/// Every Hub→Node request frame, in arrival order.
type Frames = Arc<Mutex<Vec<Value>>>;

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

/// A Node that records every Hub request and accepts it.
async fn connect_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace: &str,
    bearer: &str,
    frames: Frames,
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
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude",
                             "authState":"unknown"}],
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
        loop {
            let frame = match node.next().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                    Ok(frame) => frame,
                    Err(_) => continue,
                },
                Some(Ok(_)) => continue,
                None | Some(Err(_)) => break,
            };
            let (Some(id), Some(_)) = (frame.get("id").cloned(), frame.get("method")) else {
                continue;
            };
            frames.lock().unwrap().push(frame);
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
    frames: Frames,
    node: Option<FakeNode>,
    node_token: String,
}

fn test_config(dir: &Path) -> HubConfig {
    let mut config = HubConfig::for_test(dir.join("hub"));
    config.command_accept_timeout_ms = 500;
    config
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(test_config(dir.path())).await?;
        // The test decides when a tick runs and what time it is.
        hub.test_hold_wake_tick(true);
        let human = hub.mint_device_token("wake-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?;
        let frames = Frames::default();
        let node = connect_node(&hub, &host, &workspace, &enroll, frames.clone()).await?;
        let node_token = node.node_token.clone().context("node token")?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            frames,
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

    /// Stop the Hub and start a new one on the same data directory; the
    /// Node reconnects to it.
    async fn restart(mut self) -> Result<Ctx> {
        if self.node.take().is_some() {
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
        }
        let Ctx {
            dir,
            hub,
            human,
            host,
            workspace,
            frames,
            node_token,
            ..
        } = self;
        hub.shutdown().await;
        let hub = spawn(test_config(dir.path())).await?;
        hub.test_hold_wake_tick(true);
        let node = connect_node(&hub, &host, &workspace, &node_token, frames.clone()).await?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            frames,
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

    /// A human-seated coordinator (a continuity lineage), running and idle.
    async fn seat_with(&self, project: &str, extra: Value) -> Result<(String, String)> {
        let mut body = json!({ "role": "project-coordinator", "projectId": project,
                               "prompt": "coordinate", "driver": "claude-pty" });
        for (key, value) in extra.as_object().into_iter().flatten() {
            body[key] = value.clone();
        }
        let created = self.post(&self.human, "/v1/instances", body).await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string();
        let token = self.agent_token(&id).await?;
        self.journal(&id, entity("running", &at(0))).await?;
        self.journal(&id, activity("idle", &at(1))).await?;
        Ok((id, token))
    }

    async fn seat(&self, project: &str) -> Result<(String, String)> {
        self.seat_with(project, json!({})).await
    }

    async fn worker(&self, token: &str, project: &str) -> Result<String> {
        let created = self
            .post(
                token,
                "/v1/instances",
                json!({ "role": "worker", "projectId": project, "prompt": "work",
                        "driver": "claude-pty" }),
            )
            .await?;
        Ok(created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string())
    }

    async fn journal(&self, instance_id: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        Ok(())
    }

    /// `POST /v1/lineages/{lineage}/wake-ups` as `token`.
    async fn add(&self, token: &str, lineage: &str, body: Value) -> Result<(u16, Value)> {
        self.request(
            reqwest::Method::POST,
            token,
            &format!("/v1/lineages/{lineage}/wake-ups"),
            Some(body),
        )
        .await
    }

    async fn add_ok(&self, token: &str, lineage: &str, body: Value) -> Result<Value> {
        let (status, body) = self.add(token, lineage, body).await?;
        anyhow::ensure!(status == 201, "add {status}: {body}");
        Ok(body["wakeUp"].clone())
    }

    async fn list(&self, lineage: &str) -> Result<Value> {
        let (status, body) = self
            .request(
                reqwest::Method::GET,
                &self.human,
                &format!("/v1/lineages/{lineage}/wake-ups"),
                None,
            )
            .await?;
        anyhow::ensure!(status == 200, "list {status}: {body}");
        Ok(body)
    }

    /// One injected wake tick at `now`; returns the runs it recorded.
    async fn tick(&self, now: &str) -> Result<usize> {
        let fired = self.hub.test_wake_tick(now).await?;
        self.settle().await?;
        Ok(fired)
    }

    /// The lineage's runs, oldest first.
    async fn runs(&self, lineage: &str) -> Result<Vec<Value>> {
        let mut runs = self.list(lineage).await?["runs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        runs.reverse();
        Ok(runs)
    }

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

    /// The batch sends that carry a wake run, in order.
    fn wake_sends(&self, seat: &str) -> Vec<Value> {
        self.sends_to(seat)
            .into_iter()
            .filter(|send| text_of(send).contains(UNATTENDED))
            .collect()
    }

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

    /// The seat runs the turn the latest wake batch started and ends it with
    /// `status` (`success` or `error`), then goes idle again.
    async fn run_wake_turn(&self, seat: &str, status: &str, seconds: i64) -> Result<String> {
        let batch = self
            .wake_sends(seat)
            .last()
            .and_then(|send| send["commandId"].as_str().map(str::to_owned))
            .context("a wake batch was sent")?;
        self.journal(seat, activity("working", &at(seconds)))
            .await?;
        self.settle_command(seat, &batch).await?;
        self.journal(seat, turn_started(&batch, &at(seconds + 1)))
            .await?;
        let error = (status == "error").then_some("API error");
        self.journal(
            seat,
            turn_result(status, Some(&batch), true, error, &at(seconds + 2)),
        )
        .await?;
        self.journal(seat, activity("idle", &at(seconds + 3)))
            .await?;
        self.settle().await?;
        Ok(batch)
    }

    fn count(&self, sql: &str, param: &str) -> Result<i64> {
        Ok(self.db()?.query_row(sql, [param], |row| row.get(0))?)
    }
}

fn text_of(send: &Value) -> String {
    send["input"]["text"].as_str().unwrap_or("").to_owned()
}

// ─── journal events ─────────────────────────────────────────────────────────

const T0: &str = "2026-11-01T08:00:00.000Z";

fn shift(base: &str, seconds: i64) -> String {
    let base = time::OffsetDateTime::parse(base, &time::format_description::well_known::Rfc3339)
        .expect("time");
    let t = (base + time::Duration::seconds(seconds)).to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

fn at(seconds: i64) -> String {
    shift(T0, seconds)
}

fn entity(state: &str, observed_at: &str) -> Value {
    json!({ "kind": "lifecycle", "observedAt": observed_at,
            "payload": { "type": "entity", "entityType": "instance", "state": state } })
}

fn activity(value: &str, observed_at: &str) -> Value {
    json!({ "kind": "lifecycle", "observedAt": observed_at, "payload": {
        "type": "native", "topic": "session", "nativeName": "session",
        "nativeId": { "state": "known", "value": "session-seat" },
        "status": { "state": "known", "value": value },
        "relatedIds": {}, "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn turn_started(command: &str, observed_at: &str) -> Value {
    json!({ "kind": "lifecycle", "observedAt": observed_at, "payload": {
        "type": "native", "topic": "turn", "nativeName": "turn_started",
        "nativeId": { "state": "known", "value": "session-1" },
        "status": { "state": "known", "value": "working" },
        "relatedIds": { "nativeClientMessageId": command },
        "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn turn_result(
    status: &str,
    completed: Option<&str>,
    settled: bool,
    error: Option<&str>,
    observed_at: &str,
) -> Value {
    let mut related = serde_json::Map::new();
    if let Some(command) = completed {
        related.insert("completedTurn".into(), json!(command));
    }
    if settled {
        related.insert("settledRootTurn".into(), json!("true"));
    }
    if let Some(text) = error {
        related.insert("lastError".into(), json!(text));
    }
    json!({ "kind": "lifecycle", "observedAt": observed_at, "payload": {
        "type": "native", "topic": "turn", "nativeName": "result",
        "nativeId": { "state": "known", "value": "session-1" },
        "status": { "state": "known", "value": status },
        "relatedIds": related, "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

/// A worker turn: started, then finished, leaving `started` and `turn-done`
/// facts for its parent.
async fn worker_turn(ctx: &Ctx, worker: &str, seconds: i64) -> Result<()> {
    ctx.journal(worker, entity("running", &at(seconds))).await?;
    ctx.journal(
        worker,
        turn_started(&format!("cmd_w_{seconds}"), &at(seconds + 1)),
    )
    .await?;
    ctx.journal(
        worker,
        turn_result(
            "success",
            Some(&format!("cmd_w_{seconds}")),
            true,
            None,
            &at(seconds + 2),
        ),
    )
    .await?;
    Ok(())
}

fn every(secs: i64, name: &str, notify: &str) -> Value {
    json!({ "name": name, "text": format!("Check on {name}."),
            "trigger": { "kind": "every", "everySecs": secs }, "notify": notify })
}

fn next_fire(wake: &Value) -> String {
    wake["nextFireAt"].as_str().unwrap_or_default().to_owned()
}

// ─── triggers ───────────────────────────────────────────────────────────────

/// §11: `at`, `every` and `when` (over a fact that exists, `turn-done`) each
/// fire exactly once for their occurrence, and a second tick at the same
/// instant fires nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_every_and_when_each_fire_once() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-triggers").await?;
    let (seat, token) = ctx.seat(&project).await?;
    // Busy: runs wait in the inbox, so the counts below are the claims.
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let at_wake = ctx
        .add_ok(
            &token,
            &seat,
            json!({ "name": "once", "text": "Look at the board.",
                    "trigger": { "kind": "at", "afterSecs": 600 }, "notify": "never" }),
        )
        .await?;
    let every_wake = ctx
        .add_ok(&token, &seat, every(600, "tenmin", "never"))
        .await?;
    let when_wake = ctx
        .add_ok(
            &token,
            &seat,
            json!({ "name": "on-turn", "text": "A worker finished.",
                    "trigger": { "kind": "when", "factKind": "turn-done" }, "notify": "never" }),
        )
        .await?;
    assert_eq!(when_wake["nextFireAt"], Value::Null, "{when_wake}");
    let worker = ctx.worker(&token, &project).await?;
    worker_turn(&ctx, &worker, 10).await?;

    let due = shift(&next_fire(&at_wake).max(next_fire(&every_wake)), 1);
    assert_eq!(ctx.tick(&due).await?, 3, "one run per wake-up");
    assert_eq!(ctx.tick(&due).await?, 0, "each occurrence is claimed once");
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 3, "{runs:#?}");
    for name in ["once", "tenmin", "on-turn"] {
        let run = runs.iter().find(|run| run["name"] == name).context(name)?;
        assert_eq!(run["state"], "queued", "{run}");
        assert_eq!(run["missedBefore"], 0, "{run}");
        assert_eq!(run["checkExit"], Value::Null);
        assert!(run["factId"].is_i64(), "a wake-due fact: {run}");
    }
    let list = ctx.list(&seat).await?;
    let state = |name: &str| {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|wake| wake["name"] == name)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(state("once")["state"], "done");
    assert_eq!(state("once")["nextFireAt"], Value::Null);
    assert_eq!(
        next_fire(&state("tenmin")),
        shift(&next_fire(&every_wake), 600),
        "the next occurrence follows the schedule"
    );
    assert_eq!(state("on-turn")["state"], "active");
    // A later fact of the kind fires the `when` once more, a fact of
    // another kind does not.
    let worker2 = ctx.worker(&token, &project).await?;
    ctx.journal(&worker2, entity("running", &at(40))).await?;
    assert_eq!(ctx.tick(&due).await?, 0, "a started fact is not turn-done");
    let wake_due: i64 = ctx.count(
        "SELECT COUNT(*) FROM inbox_facts WHERE subscriber_lineage_id = ?1 AND kind = 'wake-due'",
        &seat,
    )?;
    assert_eq!(wake_due, 3);
    Ok(())
}

/// §11.1: occurrences the Hub missed fire once, for the latest, with their
/// count; an occurrence that comes due while the previous run still waits
/// is counted too, and the next run reports it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missed_occurrences_fire_once_with_their_count() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-missed").await?;
    let (seat, token) = ctx.seat(&project).await?;
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let wake = ctx
        .add_ok(&token, &seat, every(600, "tenmin", "never"))
        .await?;
    let first = next_fire(&wake);
    // Four occurrences are due (first, +10, +20, +30 min): one run.
    assert_eq!(ctx.tick(&shift(&first, 1801)).await?, 1);
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["missedBefore"], 3);
    assert_eq!(runs[0]["scheduledAt"], shift(&first, 1800).as_str());
    let listed = ctx.list(&seat).await?;
    assert_eq!(
        next_fire(&listed["items"][0]),
        shift(&first, 2400),
        "the next occurrence after the latest"
    );
    // The run still waits (the seat is busy): the next occurrence is kept
    // as a count, not a second queued run.
    assert_eq!(ctx.tick(&shift(&first, 2401)).await?, 0);
    let listed = ctx.list(&seat).await?;
    assert_eq!(listed["items"][0]["missedCount"], 1);
    assert_eq!(ctx.runs(&seat).await?.len(), 1);
    // The first run is delivered and finishes; the next one reports the
    // occurrence counted meanwhile.
    ctx.journal(&seat, activity("idle", &at(3))).await?;
    ctx.settle().await?;
    let envelope = text_of(ctx.wake_sends(&seat).last().context("first batch")?);
    assert!(
        envelope.contains("3 earlier occurrences were skipped without a run of their own"),
        "{envelope}"
    );
    ctx.run_wake_turn(&seat, "success", 10).await?;
    assert_eq!(ctx.tick(&shift(&first, 3001)).await?, 1);
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["state"], "success");
    assert_eq!(runs[1]["missedBefore"], 1, "the counted occurrence");
    assert_eq!(ctx.list(&seat).await?["items"][0]["missedCount"], 0);
    let envelope = text_of(ctx.wake_sends(&seat).last().context("second batch")?);
    assert!(
        envelope.contains("1 earlier occurrence was skipped without a run of its own"),
        "{envelope}"
    );
    Ok(())
}

/// §11.1, r2 item 1: an occurrence that comes due while the previous run is
/// still open is skipped, not deferred. No run is ever made for it, not even
/// once the open run ends; the next scheduled run reports exactly that one
/// occurrence (it is counted once, and the fired one is never counted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_occurrence_due_while_the_run_is_open_is_skipped_and_reported_once() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-skip").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let wake = ctx
        .add_ok(&token, &seat, every(600, "beat", "never"))
        .await?;
    let first = next_fire(&wake);
    // The first occurrence fires; its batch goes out at once: running.
    assert_eq!(ctx.tick(&shift(&first, 1)).await?, 1);
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "running", "{runs:#?}");
    assert_eq!(runs[0]["missedBefore"], 0);
    // The next occurrence comes due while that run is open: no run, and it
    // is counted once.
    assert_eq!(ctx.tick(&shift(&first, 601)).await?, 0);
    let listed = ctx.list(&seat).await?;
    assert_eq!(listed["items"][0]["missedCount"], 1);
    assert_eq!(next_fire(&listed["items"][0]), shift(&first, 1200));
    // The open run ends. The skipped occurrence is not delivered late.
    ctx.run_wake_turn(&seat, "success", 10).await?;
    assert_eq!(ctx.runs(&seat).await?[0]["state"], "success");
    assert_eq!(
        ctx.tick(&shift(&first, 630)).await?,
        0,
        "skipped, not deferred"
    );
    assert_eq!(ctx.runs(&seat).await?.len(), 1);
    // The occurrence after it fires and reports the one it skipped.
    assert_eq!(ctx.tick(&shift(&first, 1201)).await?, 1);
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 2, "{runs:#?}");
    assert_eq!(runs[1]["scheduledAt"], shift(&first, 1200).as_str());
    assert_eq!(runs[1]["missedBefore"], 1);
    assert!(
        runs.iter()
            .all(|run| run["scheduledAt"] != shift(&first, 600).as_str()),
        "no run for the skipped occurrence: {runs:#?}"
    );
    assert_eq!(ctx.list(&seat).await?["items"][0]["missedCount"], 0);
    let envelope = text_of(ctx.wake_sends(&seat).last().context("second batch")?);
    assert!(
        envelope.contains("1 earlier occurrence was skipped without a run of its own"),
        "{envelope}"
    );
    Ok(())
}

// ─── delivery ───────────────────────────────────────────────────────────────

/// §11.1, §11.3: a due run on a busy lineage waits in the inbox (no send,
/// no separate deferral); at the idle point it goes out as one hub-origin
/// batch that opens with the run's envelope; the turn it starts settles the
/// run. A hub-origin turn notifies nothing under `never`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_lineage_waits_for_idle_through_the_inbox() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-busy").await?;
    let (seat, token) = ctx.seat(&project).await?;
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let wake = ctx
        .add_ok(
            &token,
            &seat,
            json!({ "name": "standup", "text": "Summarize the night's gate runs.\nKeep it short.",
                    "trigger": { "kind": "at", "afterSecs": 120 }, "notify": "never" }),
        )
        .await?;
    assert_eq!(ctx.tick(&shift(&next_fire(&wake), 2)).await?, 1);
    assert!(ctx.sends_to(&seat).is_empty(), "busy: nothing sent");
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "queued");
    let fact_id = runs[0]["factId"].as_i64().context("fact")?;
    let inbox = ctx
        .request(
            reqwest::Method::GET,
            &token,
            &format!("/v1/lineages/{seat}/inbox"),
            None,
        )
        .await?
        .1;
    let fact = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fact| fact["id"] == fact_id)
        .context("the wake-due row")?;
    assert_eq!(fact["kind"], "wake-due");
    assert!(fact["batchId"].is_null(), "waits: {fact}");

    ctx.journal(&seat, activity("idle", &at(30))).await?;
    ctx.settle().await?;
    let sends = ctx.sends_to(&seat);
    assert_eq!(sends.len(), 1, "one batch: {sends:#?}");
    let send = &sends[0];
    assert_eq!(send["origin"], "hub");
    let text = text_of(send);
    assert!(
        text.starts_with("<remuda-facts> Generated by the Remuda Hub, not by the owner: a wake-up")
    );
    let run_id = runs[0]["runId"].as_str().unwrap();
    let wake_id = wake["wakeId"].as_str().unwrap();
    for needle in [
        wake_id,
        "standup",
        run_id,
        &format!("fired at {}", runs[0]["firedAt"].as_str().unwrap()),
        "no earlier occurrence was skipped",
        UNATTENDED,
        END_TURN,
        r#""Summarize the night's gate runs.\nKeep it short.""#,
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert!(!text.to_lowercase().contains("authoriz"), "{text}");
    let batch = send["commandId"].as_str().unwrap();
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "running");
    assert_eq!(runs[0]["batchId"], batch);
    let template: i64 = ctx.db()?.query_row(
        "SELECT template_version FROM inbox_batches WHERE batch_id = ?1",
        [batch],
        |row| row.get(0),
    )?;
    assert_eq!(template, 2);

    ctx.run_wake_turn(&seat, "success", 40).await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "success", "{runs:#?}");
    assert!(runs[0]["endedAt"].is_string());
    assert_eq!(
        ctx.count(
            "SELECT COUNT(*) FROM owner_notices WHERE lineage_id = ?1",
            &seat
        )?,
        0,
        "notify never: silent"
    );
    Ok(())
}

/// §11.3: each delivered run opens with its own envelope: two runs due at
/// once go out in two batches, one per turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_due_runs_go_out_one_envelope_per_batch() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-two").await?;
    let (seat, token) = ctx.seat(&project).await?;
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let a = ctx.add_ok(&token, &seat, every(600, "a", "never")).await?;
    ctx.add_ok(&token, &seat, every(600, "b", "never")).await?;
    ctx.journal(&seat, activity("idle", &at(3))).await?;
    assert_eq!(ctx.tick(&shift(&next_fire(&a), 5)).await?, 2);
    assert_eq!(ctx.wake_sends(&seat).len(), 1, "one run per batch");
    ctx.run_wake_turn(&seat, "success", 10).await?;
    let sends = ctx.wake_sends(&seat);
    assert_eq!(sends.len(), 2, "the second run follows: {sends:#?}");
    assert_eq!(text_of(&sends[1]).matches(UNATTENDED).count(), 1);
    ctx.run_wake_turn(&seat, "success", 20).await?;
    let runs = ctx.runs(&seat).await?;
    assert!(
        runs.iter().all(|run| run["state"] == "success"),
        "{runs:#?}"
    );
    Ok(())
}

// ─── restart ────────────────────────────────────────────────────────────────

/// §11.1: a Hub restart marks a running run interrupted and never replays
/// it — not even when its batch is later handed back as never dispatched —
/// and the next occurrence follows the schedule. An `at` due after the
/// restart fires once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_restart_interrupts_a_running_run_and_never_replays_it() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-restart").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let wake = ctx
        .add_ok(&token, &seat, every(600, "tenmin", "never"))
        .await?;
    let once = ctx
        .add_ok(
            &token,
            &seat,
            json!({ "name": "once", "text": "After the restart.",
                    "trigger": { "kind": "at", "afterSecs": 3600 }, "notify": "never" }),
        )
        .await?;
    let first = next_fire(&wake);
    assert_eq!(ctx.tick(&shift(&first, 1)).await?, 1);
    let sends = ctx.wake_sends(&seat);
    assert_eq!(sends.len(), 1);
    let old_batch = sends[0]["commandId"].as_str().unwrap().to_owned();
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "running", "forwarded: {runs:#?}");
    let old_run = runs[0]["runId"].as_str().unwrap().to_owned();

    let ctx = ctx.restart().await?;
    ctx.journal(&seat, activity("idle", &at(50))).await?;
    ctx.settle().await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "interrupted", "{runs:#?}");
    assert!(runs[0]["endedAt"].is_string());
    assert_eq!(ctx.wake_sends(&seat).len(), 1, "nothing replayed");

    // The Node later says it never dispatched that batch: its rows would go
    // back, but the interrupted run's fact stays consumed.
    ctx.journal(
        &seat,
        json!({ "kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "command", "entityId": old_batch,
            "state": "settled",
            "entity": { "commandId": old_batch, "state": "settled",
                        "settlement": { "state": "known", "value": {
                            "outcome": "rejected",
                            "error": { "message": "instance-queue-full",
                                       "execution": "not-dispatched" } } } } } }),
    )
    .await?;
    ctx.settle().await?;
    let fact_id = runs[0]["factId"].as_i64().unwrap();
    let batch_of_fact: Option<String> = ctx.db()?.query_row(
        "SELECT batch_id FROM inbox_facts WHERE id = ?1",
        [fact_id],
        |row| row.get(0),
    )?;
    assert_eq!(batch_of_fact.as_deref(), Some(old_batch.as_str()), "kept");
    assert_eq!(ctx.wake_sends(&seat).len(), 1, "still nothing replayed");
    assert_eq!(ctx.runs(&seat).await?[0]["state"], "interrupted");

    // The next occurrence is the schedule's, and it runs.
    let listed = ctx.list(&seat).await?;
    let tenmin = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|wake| wake["name"] == "tenmin")
        .context("tenmin")?;
    assert_eq!(next_fire(tenmin), shift(&first, 600));
    assert_eq!(ctx.tick(&shift(&first, 601)).await?, 1);
    let sends = ctx.wake_sends(&seat);
    assert_eq!(sends.len(), 2, "{sends:#?}");
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[1]["runId"], old_run.as_str());
    assert!(text_of(&sends[1]).contains(runs[1]["runId"].as_str().unwrap()));
    assert!(!text_of(&sends[1]).contains(&old_run));
    ctx.run_wake_turn(&seat, "success", 60).await?;

    // The `at` added before the restart fires once after it.
    let due = shift(&next_fire(&once), 1);
    ctx.tick(&due).await?;
    ctx.tick(&shift(&due, 3600)).await?;
    let runs = ctx.runs(&seat).await?;
    let once_runs = runs.iter().filter(|run| run["name"] == "once").count();
    assert_eq!(once_runs, 1, "once only: {runs:#?}");
    Ok(())
}

// ─── fresh mode ─────────────────────────────────────────────────────────────

/// §11.4: a fresh run creates a short-lived child under the lineage through
/// the ordinary Agent create (the chapter is its parent and initiator, the
/// Hub's turn its root turn), briefed with the envelope and the text; the
/// child's facts come back to the lineage and its turn settles the run. A
/// lineage without `dispatch` is refused by the same admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_mode_creates_a_child_through_admission_and_its_facts_come_back() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-fresh").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let wake = ctx
        .add_ok(
            &token,
            &seat,
            json!({ "name": "digest", "text": "Write the nightly digest.",
                    "trigger": { "kind": "at", "afterSecs": 60 }, "mode": "fresh",
                    "notify": "failure" }),
        )
        .await?;
    assert_eq!(wake["mode"], "fresh");
    assert_eq!(ctx.tick(&shift(&next_fire(&wake), 1)).await?, 1);
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "running", "{runs:#?}");
    assert!(runs[0]["factId"].is_null(), "fresh mode wakes no chapter");
    let child = runs[0]["childInstanceId"]
        .as_str()
        .context("the child is recorded")?
        .to_owned();
    assert!(
        ctx.sends_to(&seat).is_empty(),
        "the live chapter is untouched"
    );

    let (parent, driver, origin, title): (String, String, String, String) = ctx.db()?.query_row(
        "SELECT json_extract(spec_json, '$.parentInstanceId'), driver,
                    json_extract(spec_json, '$.origin'), title
               FROM instances WHERE id = ?1",
        [&child],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(parent, seat, "a child of the live chapter");
    assert_eq!(driver, "claude-print");
    assert_eq!(origin, "agent", "an ordinary Agent create");
    assert!(title.contains("digest"), "{title}");
    let (initiator, root_turn): (String, String) = ctx.db()?.query_row(
        "SELECT initiator_instance_id, initiator_root_turn_json FROM commands
          WHERE instance_id = ?1 AND operation = 'instance.create'",
        [&child],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(initiator, seat);
    let root_turn: Value = serde_json::from_str(&root_turn)?;
    assert_eq!(root_turn["origin"], "hub", "{root_turn}");
    let create = ctx
        .frames
        .lock()
        .unwrap()
        .iter()
        .find(|frame| {
            frame["method"] == "instance.create" && frame["params"]["instanceId"] == child
        })
        .cloned()
        .context("the create reached the Node")?;
    let prompt = create.to_string();
    assert!(prompt.contains("<remuda-wake>"), "{prompt}");
    assert!(prompt.contains("Write the nightly digest."), "{prompt}");

    // The child runs its one turn; its facts reach the lineage.
    ctx.journal(&child, entity("running", &at(10))).await?;
    ctx.journal(&child, turn_started("cmd_child", &at(11)))
        .await?;
    ctx.journal(
        &child,
        turn_result("success", Some("cmd_child"), true, None, &at(12)),
    )
    .await?;
    ctx.settle().await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "success", "{runs:#?}");
    let kinds: Vec<String> = {
        let db = ctx.db()?;
        let mut stmt = db.prepare(
            "SELECT kind FROM inbox_facts WHERE subscriber_lineage_id = ?1
              AND subject_instance_id = ?2 ORDER BY id",
        )?;
        stmt.query_map([&seat, &child], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    assert!(kinds.contains(&"started".to_owned()), "{kinds:?}");
    assert!(kinds.contains(&"turn-done".to_owned()), "{kinds:?}");

    // A lineage without `dispatch` is refused by the create's admission.
    let (bare, bare_token) = ctx
        .seat_with(
            &ctx.project("wake-fresh-bare").await?,
            json!({ "grants": ["land"] }),
        )
        .await?;
    let refused = ctx
        .add_ok(
            &bare_token,
            &bare,
            json!({ "name": "digest", "text": "Write the nightly digest.",
                    "trigger": { "kind": "at", "afterSecs": 60 }, "mode": "fresh",
                    "notify": "failure" }),
        )
        .await?;
    assert_eq!(ctx.tick(&shift(&next_fire(&refused), 1)).await?, 1);
    let runs = ctx.runs(&bare).await?;
    assert_eq!(runs[0]["state"], "failed", "{runs:#?}");
    assert_eq!(runs[0]["failureKind"], "fresh-refused");
    assert!(runs[0]["childInstanceId"].is_null());
    let children: i64 = ctx.count(
        "SELECT COUNT(*) FROM instances WHERE json_extract(spec_json, '$.parentInstanceId') = ?1",
        &bare,
    )?;
    assert_eq!(children, 0, "nothing was created");
    assert_eq!(
        ctx.count(
            "SELECT COUNT(*) FROM owner_notices WHERE lineage_id = ?1",
            &bare
        )?,
        1,
        "notify failure: the refusal is pushed"
    );
    Ok(())
}

// ─── notify ─────────────────────────────────────────────────────────────────

/// §11.5: `never` notifies nothing, `failure` a failed run once per
/// unrecovered failure kind, `always` every run (a repeating failure only
/// once until a run succeeds). The latch counts the repeats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_notify_matrix_and_the_repeat_failure_latch() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let mut seats = Vec::new();
    for notify in ["never", "failure", "always"] {
        // One dispatcher per project.
        let project = ctx.project(&format!("wake-notify-{notify}")).await?;
        let (seat, token) = ctx.seat(&project).await?;
        let wake = ctx
            .add_ok(&token, &seat, every(600, "beat", notify))
            .await?;
        seats.push((notify, seat, wake));
    }
    let first = seats
        .iter()
        .map(|(_, _, wake)| next_fire(wake))
        .max()
        .unwrap();
    // fail, fail again (a repeat), succeed (recover), fail.
    for (step, status) in ["error", "error", "success", "error"].iter().enumerate() {
        let now = shift(&first, 600 * step as i64 + 1);
        ctx.tick(&now).await?;
        for (_, seat, _) in &seats {
            ctx.run_wake_turn(seat, status, 100 * (step as i64 + 1))
                .await?;
        }
    }
    let notices = |seat: &str| -> Result<Vec<(String, String)>> {
        let db = ctx.db()?;
        let mut stmt = db.prepare(
            "SELECT title, source FROM owner_notices WHERE lineage_id = ?1 ORDER BY created_at",
        )?;
        Ok(stmt
            .query_map([seat], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    for (notify, seat, _) in &seats {
        let runs = ctx.runs(seat).await?;
        let states: Vec<&str> = runs
            .iter()
            .map(|run| run["state"].as_str().unwrap())
            .collect();
        assert_eq!(
            states,
            ["failed", "failed", "success", "failed"],
            "{notify}"
        );
        assert!(
            runs.iter()
                .filter(|run| run["state"] == "failed")
                .all(|run| run["failureKind"] == "turn-failed")
        );
        let titles: Vec<String> = notices(seat)?.into_iter().map(|(title, _)| title).collect();
        let expected: Vec<&str> = match *notify {
            "never" => vec![],
            "failure" => vec!["Wake-up beat failed: turn-failed"; 2],
            _ => vec![
                "Wake-up beat failed: turn-failed",
                "Wake-up beat ran",
                "Wake-up beat failed: turn-failed",
            ],
        };
        assert_eq!(titles, expected, "{notify}");
        assert!(notices(seat)?.iter().all(|(_, source)| source == "wake"));
    }
    // The latch of the last failure is set and counted no repeat yet.
    let (_, failing, wake) = &seats[1];
    let (repeats, since): (i64, String) = ctx.db()?.query_row(
        "SELECT repeats, since FROM owner_notify_latches WHERE scope = ?1",
        [format!("wake:{}", wake["wakeId"].as_str().unwrap())],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(repeats, 0, "latched anew after the recovery ({since})");
    assert!(!failing.is_empty());
    Ok(())
}

/// §11.1: when persisting a run's result fails, only the write is retried:
/// the run is not run again, and its notice is claimed once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_result_write_is_retried_never_the_run() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-retry").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let wake = ctx
        .add_ok(&token, &seat, every(600, "beat", "always"))
        .await?;
    ctx.tick(&shift(&next_fire(&wake), 1)).await?;
    let run_id = ctx.runs(&seat).await?[0]["runId"]
        .as_str()
        .unwrap()
        .to_owned();
    // Every write of the result fails until the seam is released.
    remuda_hub::wake_test_support::arm_result_failure(&run_id, 1000);
    ctx.run_wake_turn(&seat, "success", 10).await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "running", "the write failed: {runs:#?}");
    let attempts: i64 = ctx.db()?.query_row(
        "SELECT attempts FROM wake_outcomes WHERE run_id = ?1",
        [&run_id],
        |row| row.get(0),
    )?;
    assert!(attempts >= 1, "the result waits to be written ({attempts})");
    remuda_hub::wake_test_support::arm_result_failure(&run_id, 0);
    // The next try comes after its backoff: the tick's clock is past it.
    ctx.tick(&shift(&next_fire(&wake), 300)).await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 1, "never run again");
    assert_eq!(runs[0]["state"], "success");
    assert_eq!(ctx.wake_sends(&seat).len(), 1, "one delivery");
    assert_eq!(
        ctx.count(
            "SELECT COUNT(*) FROM wake_outcomes WHERE run_id = ?1",
            &run_id
        )?,
        0
    );
    assert_eq!(
        ctx.count(
            "SELECT COUNT(*) FROM owner_notices WHERE lineage_id = ?1",
            &seat
        )?,
        1
    );
    Ok(())
}

/// §11.1 at boot: a run whose result was already queued (its write kept
/// failing) is not interrupted — the result is evidence the run ended — and
/// the next pass writes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_result_survives_a_restart_and_is_written_after_it() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-boot-result").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let wake = ctx
        .add_ok(&token, &seat, every(600, "beat", "never"))
        .await?;
    ctx.tick(&shift(&next_fire(&wake), 1)).await?;
    let run_id = ctx.runs(&seat).await?[0]["runId"]
        .as_str()
        .unwrap()
        .to_owned();
    remuda_hub::wake_test_support::arm_result_failure(&run_id, 1000);
    ctx.run_wake_turn(&seat, "success", 10).await?;
    assert_eq!(ctx.runs(&seat).await?[0]["state"], "running");
    let ctx = ctx.restart().await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "running", "not interrupted: {runs:#?}");
    remuda_hub::wake_test_support::arm_result_failure(&run_id, 0);
    ctx.tick(&shift(&next_fire(&wake), 300)).await?;
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs[0]["state"], "success", "{runs:#?}");
    assert_eq!(ctx.wake_sends(&seat).len(), 1, "never run again");
    Ok(())
}

/// r2 item 4: once its lineage is gone, a `when` wake-up turns `done` on the
/// next tick (no fact can ever reach it again), and a scheduled one when it
/// next comes due; neither fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_ups_of_a_deleted_lineage_turn_done() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-gone").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let when = ctx
        .add_ok(
            &token,
            &seat,
            json!({ "name": "on-gate", "text": "A gate ended.",
                    "trigger": { "kind": "when", "factKind": "gate" }, "notify": "never" }),
        )
        .await?;
    let scheduled = ctx
        .add_ok(&token, &seat, every(600, "beat", "never"))
        .await?;
    // The seat's process ends; the owner deletes the lineage.
    ctx.journal(&seat, entity("exited", &at(5))).await?;
    let (status, body) = ctx
        .request(
            reqwest::Method::DELETE,
            &ctx.human,
            &format!("/v1/instances/{seat}"),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let state = |wake: &Value| -> Result<String> {
        Ok(ctx.db()?.query_row(
            "SELECT state FROM wake_ups WHERE wake_id = ?1",
            [wake["wakeId"].as_str().unwrap()],
            |row| row.get(0),
        )?)
    };
    assert_eq!(state(&when)?, "active");
    // A tick before the scheduled one is due: the `when` one is done.
    assert_eq!(ctx.tick(&shift(&next_fire(&scheduled), -60)).await?, 0);
    assert_eq!(state(&when)?, "done");
    assert_eq!(state(&scheduled)?, "active");
    // The scheduled one turns done when it comes due, without a run.
    assert_eq!(ctx.tick(&shift(&next_fire(&scheduled), 1)).await?, 0);
    assert_eq!(state(&scheduled)?, "done");
    let runs: i64 = ctx.count(
        "SELECT COUNT(*) FROM wake_runs WHERE lineage_id = ?1",
        &seat,
    )?;
    assert_eq!(runs, 0, "nothing fired");
    Ok(())
}

// ─── the routes ─────────────────────────────────────────────────────────────

/// §11.1: `add --name` is idempotent: the same add returns the stored
/// wake-up (200, `created: false`); a different one under the same name is
/// refused; adds without a name are distinct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_with_a_name_is_idempotent() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-name").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let body = json!({ "name": "standup", "text": "Post the standup.",
                       "trigger": { "kind": "at", "afterSecs": 300 }, "notify": "always" });
    let (status, first) = ctx.add(&token, &seat, body.clone()).await?;
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["created"], true);
    let (status, again) = ctx.add(&token, &seat, body.clone()).await?;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["created"], false);
    assert_eq!(
        again["wakeUp"], first["wakeUp"],
        "the stored row, unchanged"
    );
    let mut other = body.clone();
    other["text"] = json!("Something else.");
    let (status, refused) = ctx.add(&token, &seat, other).await?;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["reason"], "wake-name-taken");
    let unnamed = json!({ "text": "x", "trigger": { "kind": "at", "afterSecs": 300 },
                          "notify": "never" });
    let a = ctx.add_ok(&token, &seat, unnamed.clone()).await?;
    let b = ctx.add_ok(&token, &seat, unnamed).await?;
    assert_ne!(a["name"], b["name"]);
    assert_eq!(ctx.list(&seat).await?["items"].as_array().unwrap().len(), 3);
    // rm by name, then the name is free again.
    let (status, removed) = ctx
        .request(
            reqwest::Method::DELETE,
            &token,
            &format!("/v1/lineages/{seat}/wake-ups/standup"),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{removed}");
    assert_eq!(removed["wakeId"], first["wakeUp"]["wakeId"]);
    assert_eq!(ctx.add(&token, &seat, body).await?.0, 201);
    Ok(())
}

/// §11 CLI rule: an Agent manages its own lineage's wake-ups only; the owner
/// manages any; a Bot credential none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_cannot_manage_another_lineages_wake_ups() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let (mine, my_token) = ctx.seat(&ctx.project("wake-mine").await?).await?;
    let (theirs, their_token) = ctx.seat(&ctx.project("wake-theirs").await?).await?;
    let body = json!({ "name": "w", "text": "t", "trigger": { "kind": "at", "afterSecs": 60 },
                       "notify": "never" });
    let theirs_wake = ctx.add_ok(&their_token, &theirs, body.clone()).await?;
    for (method, path, body) in [
        (
            reqwest::Method::POST,
            format!("/v1/lineages/{theirs}/wake-ups"),
            Some(body.clone()),
        ),
        (
            reqwest::Method::GET,
            format!("/v1/lineages/{theirs}/wake-ups"),
            None,
        ),
        (
            reqwest::Method::DELETE,
            format!("/v1/lineages/{theirs}/wake-ups/w"),
            None,
        ),
    ] {
        let (status, refused) = ctx.request(method.clone(), &my_token, &path, body).await?;
        assert_eq!(status, 403, "{method} {path}: {refused}");
    }
    assert_eq!(
        ctx.list(&theirs).await?["items"][0]["wakeId"],
        theirs_wake["wakeId"],
        "untouched"
    );
    // Its own lineage, and the owner for any.
    assert_eq!(ctx.add(&my_token, &mine, body.clone()).await?.0, 201);
    let mut owners = body.clone();
    owners["name"] = json!("from-owner");
    let added = ctx.add_ok(&ctx.human, &theirs, owners).await?;
    assert_eq!(added["creator"]["origin"], "human");
    let bot = ctx.hub.mint_bot_device_token("wake-bot").await?;
    assert_eq!(ctx.add(&bot, &theirs, body).await?.0, 403);
    Ok(())
}

/// The add is bounded: a credential in the text is refused as one, a check
/// is "not supported yet", a context-percentage condition names its missing
/// source, and an interval out of range is a 400. Nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_add_refuses_what_it_cannot_keep() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-bounds").await?;
    let (seat, token) = ctx.seat(&project).await?;
    let key = ["sk", "-ant-api03-", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"].concat();
    let (status, body) = ctx
        .add(
            &token,
            &seat,
            json!({ "text": format!("use {key}"), "trigger": { "kind": "at", "afterSecs": 1 },
                    "notify": "never" }),
        )
        .await?;
    assert_eq!(status, 422);
    assert_eq!(body["code"], "WAKE_SECRET_REJECTED");
    assert!(!body.to_string().contains(&key), "never echoed");
    let (status, body) = ctx
        .add(
            &token,
            &seat,
            json!({ "text": "t", "trigger": { "kind": "at", "afterSecs": 1 },
                    "check": { "command": "true", "timeoutSecs": 5 }, "notify": "never" }),
        )
        .await?;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["reason"], "check-unsupported");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not supported yet")
    );
    let (status, body) = ctx
        .add(
            &token,
            &seat,
            json!({ "text": "t", "trigger": { "kind": "when", "factKind": "context-threshold" },
                    "notify": "never" }),
        )
        .await?;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["reason"], "condition-unsupported");
    let (status, body) = ctx
        .add(&token, &seat, every(i64::MAX, "huge", "never"))
        .await?;
    assert_eq!(status, 400, "{body}");
    let (status, _) = ctx
        .add(
            &token,
            &seat,
            json!({ "text": "t", "trigger": { "kind": "at", "afterSecs": 1 } }),
        )
        .await?;
    assert!(status >= 400, "notify is declared, never defaulted");
    assert_eq!(ctx.list(&seat).await?["items"], json!([]));
    Ok(())
}

/// A stored interval of i64::MAX (a hand edit, a restore from an older
/// build) never panics the tick: that wake-up is skipped, every other one
/// fires, and the Hub keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interval_of_i64_max_does_not_panic_the_tick() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-max").await?;
    let (seat, token) = ctx.seat(&project).await?;
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let broken = ctx
        .add_ok(&token, &seat, every(600, "broken", "never"))
        .await?;
    let fine = ctx
        .add_ok(&token, &seat, every(600, "fine", "never"))
        .await?;
    ctx.db()?.execute(
        "UPDATE wake_ups SET every_secs = ?2 WHERE wake_id = ?1",
        rusqlite::params![broken["wakeId"].as_str().unwrap(), i64::MAX],
    )?;
    let now = shift(&next_fire(&fine), 3600);
    assert_eq!(ctx.tick(&now).await?, 1, "the other wake-up fires");
    assert_eq!(ctx.tick(&now).await?, 0);
    let runs = ctx.runs(&seat).await?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["name"], "fine");
    // The Hub still serves and still ticks.
    assert_eq!(ctx.list(&seat).await?["items"].as_array().unwrap().len(), 2);
    assert_eq!(ctx.tick(&shift(&now, 600)).await?, 0, "busy: counted");
    Ok(())
}

// ─── /healthz ───────────────────────────────────────────────────────────────

/// `/healthz` reports the last tick and the delivery age, and answers 503
/// once no tick completed for the stale bound (12 tick periods).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_answers_503_when_the_tick_is_stale() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-health").await?;
    let (seat, token) = ctx.seat(&project).await?;
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let wake = ctx
        .add_ok(&token, &seat, every(600, "beat", "never"))
        .await?;
    ctx.tick(&shift(&next_fire(&wake), 1)).await?;
    let health = |ctx: &Ctx| {
        let url = format!("{}/healthz", ctx.base());
        let http = ctx.http.clone();
        async move {
            let response = http.get(url).send().await?;
            let status = response.status().as_u16();
            Ok::<_, anyhow::Error>((status, response.json::<Value>().await?))
        }
    };
    let (status, body) = health(&ctx).await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert!(body["lastTickAt"].is_string(), "{body}");
    assert_eq!(body["tickPeriodSecs"], 5);
    let stale_after = remuda_hub::wake_test_support::stale_after_secs();
    assert_eq!(body["tickStaleAfterSecs"], stale_after);
    assert!(body["tickAgeSecs"].as_u64().unwrap() <= stale_after);
    assert!(body["deliveryAgeSecs"].is_u64(), "a run waits: {body}");

    ctx.hub
        .test_backdate_wake_tick(i64::try_from(stale_after).unwrap() + 5);
    let (status, body) = health(&ctx).await?;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "HUB_TICK_STALE");
    assert!(body["tickAgeSecs"].as_u64().unwrap() > stale_after);

    ctx.tick(&shift(&next_fire(&wake), 2)).await?;
    let (status, body) = health(&ctx).await?;
    assert_eq!(status, 200, "a completed tick: {body}");
    Ok(())
}

// ─── backup ─────────────────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A byte-copying stand-in for `age` (`-e -r R -o OUT IN` / `-d -i K -o OUT IN`).
fn fake_age(bin: &Path) -> Result<()> {
    std::fs::create_dir_all(bin)?;
    remuda_testing::install_at(
        &bin.join("age"),
        "#!/bin/sh\n\
         # Test stand-in: copies bytes; the backup test checks members, not crypto.\n\
         out=''; in=''\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -r|-i) shift 2 ;;\n\
             -e|-d) shift ;;\n\
             *) in=\"$1\"; shift ;;\n\
           esac\n\
         done\n\
         cat \"$in\" > \"$out\"\n",
    )?;
    Ok(())
}

fn run(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    anyhow::ensure!(
        output.status.success(),
        "{command:?} failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(stdout)
}

/// The rows ride `hub.sqlite`, so `scripts/hub-backup.sh` and a restore carry
/// them: the restored Hub lists the same wake-ups and runs, and an add under
/// the same name is still a repeat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_ups_survive_backup_and_restore() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("wake-backup").await?;
    let (seat, token) = ctx.seat(&project).await?;
    ctx.journal(&seat, activity("working", &at(2))).await?;
    let body = every(600, "kept", "failure");
    let wake = ctx.add_ok(&token, &seat, body.clone()).await?;
    ctx.tick(&shift(&next_fire(&wake), 1)).await?;
    let before = ctx.list(&seat).await?;
    assert_eq!(before["runs"].as_array().unwrap().len(), 1);
    let data = ctx.dir.path().join("hub");
    let Ctx { dir, hub, node, .. } = ctx;
    drop(node);
    hub.shutdown().await;
    std::fs::create_dir_all(data.join("secrets"))?;
    if !data.join("secrets/master.key").exists() {
        std::fs::write(data.join("secrets/master.key"), "test-master-key")?;
    }
    if !data.join("bootstrap-token").exists() {
        std::fs::write(data.join("bootstrap-token"), "test-bootstrap")?;
    }
    let bin = dir.path().join("bin");
    fake_age(&bin)?;
    let out = dir.path().join("backups");
    let scratch = dir.path().join("tmp");
    std::fs::create_dir_all(&scratch)?;
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let report = run(Command::new("bash")
        .arg(repo_root().join("scripts/hub-backup.sh"))
        .arg("--data-dir")
        .arg(&data)
        .arg("--output-dir")
        .arg(&out)
        .args(["--recipient", "age1testrecipient", "--execute"])
        .env("REMUDA_BACKUP_YES_I_KNOW", "1")
        .env("PATH", &path)
        .env("TMPDIR", &scratch))?;
    assert!(report.contains("execute ok"), "{report}");
    let bundle = std::fs::read_dir(&out)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|ext| ext == "age"))
        .context("bundle")?;
    let tar = dir.path().join("bundle.tar");
    run(Command::new(bin.join("age"))
        .args(["-d", "-i", "key", "-o"])
        .arg(&tar)
        .arg(&bundle))?;
    let staged = dir.path().join("staged");
    std::fs::create_dir_all(&staged)?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(&tar)
        .arg("-C")
        .arg(&staged))?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(staged.join("manifest.json"))?)?;
    let restored = dir.path().join("restored");
    std::fs::create_dir_all(&restored)?;
    for member in manifest["members"].as_array().context("members")? {
        let rel = member["path"].as_str().context("path")?;
        let target = restored.join(rel);
        std::fs::create_dir_all(target.parent().context("parent")?)?;
        std::fs::copy(staged.join(rel), &target)?;
    }
    let hub = spawn(HubConfig::for_test(restored)).await?;
    hub.test_hold_wake_tick(true);
    let human = hub.mint_device_token("wake-restored").await?;
    let http = reqwest::Client::new();
    let after: Value = http
        .get(format!("http://{}/v1/lineages/{seat}/wake-ups", hub.addr))
        .bearer_auth(&human)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(after["items"], before["items"], "the wake-ups survive");
    assert_eq!(after["runs"], before["runs"], "the queued run survives");
    let again = http
        .post(format!("http://{}/v1/lineages/{seat}/wake-ups", hub.addr))
        .bearer_auth(&human)
        .json(&body)
        .send()
        .await?;
    assert_eq!(again.status().as_u16(), 200, "still a repeat");
    hub.shutdown().await;
    drop(dir);
    Ok(())
}
