//! cb-inbox-facts (D-057 §10, D-060 §10.1): the Hub inbox of facts, against
//! the in-process Hub with one fake Node.
//!
//! Facts are driven the way production drives them: journal appends through
//! the store, the Hub's own commands over HTTP, a Hub restart on the same data
//! directory. Every read goes through `GET /v1/lineages/{id}/inbox`, so the
//! cases also pin the lineage read predicate. Journal times are fixed in the
//! events (`observedAt`); nothing waits on a wall-clock threshold.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// ─── fake Node ──────────────────────────────────────────────────────────────

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
    match tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
    }
}

/// Connect a Node that accepts every Hub request. `bearer` is an enroll
/// token on first contact and the returned node token afterwards.
async fn connect_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace: &str,
    bearer: &str,
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
                    "maxInstances": 8,
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
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if let Some(id) = frame.get("id").cloned()
                && frame.get("method").is_some()
            {
                let response =
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "accepted": true } });
                if node
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
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
    node: Option<FakeNode>,
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        Self::spawn_with(|_| {}).await
    }

    /// [`Self::spawn`] with the Hub config mutated first (the config a
    /// production Hub loads is the operator's; a test writes it directly).
    async fn spawn_with(mutate: impl FnOnce(&mut HubConfig)) -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let mut config = HubConfig::for_test(dir.path().join("hub"));
        mutate(&mut config);
        let hub = spawn(config).await?;
        let human = hub.mint_device_token("inbox-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?;
        let node = connect_node(&hub, &host, &workspace, &enroll).await?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            node: Some(node),
        })
    }

    /// Drop the Node's link and wait until the Hub has recorded the loss
    /// (the host row reads offline; its fact commits in the same write).
    async fn disconnect(&mut self) -> Result<()> {
        if self.node.take().is_none() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (_, hosts) = self
                .request(reqwest::Method::GET, &self.human, "/v1/hosts", None)
                .await?;
            let online = hosts
                .as_array()
                .or_else(|| hosts["items"].as_array())
                .into_iter()
                .flatten()
                .find(|host| {
                    host["hostId"] == self.host.as_str() || host["id"] == self.host.as_str()
                })
                .map(|host| host["online"] == true || host["state"] == "online");
            if online == Some(false) {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "host never offline: {hosts}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Stop the Hub and start a new one on the same data directory: the
    /// tokens, rows and journal survive; in-memory state does not. The Node
    /// link is dropped (and its loss recorded) first.
    async fn restart(mut self) -> Result<Ctx> {
        self.disconnect().await?;
        let Ctx {
            dir,
            hub,
            human,
            host,
            workspace,
            ..
        } = self;
        hub.shutdown().await;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            node: None,
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("hub")
    }

    /// A second connection to the Hub's database, for a test that must store
    /// a value the routes would refuse.
    fn db(&self) -> Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.data_dir().join("hub.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
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

    /// A human-seated project coordinator (a continuity lineage: it holds
    /// grants); returns its instance id (= lineage id) and agent token.
    async fn coordinator(&self, project: &str) -> Result<(String, String)> {
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
        Ok((id, token))
    }

    /// A worker the coordinator behind `token` dispatched.
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

    /// A plain instance written by an internal path with `spec` (a D-026
    /// resume child, a fleet fan-out): no grants, no lineage row.
    async fn plain_instance(&self, spec: Value) -> Result<String> {
        let record = self
            .hub
            .store()
            .context("store")?
            .insert_instance(
                self.host.clone(),
                Some(self.workspace.clone()),
                "claude".into(),
                "claude-pty".into(),
                None,
                spec,
            )
            .await?;
        Ok(record.instance_id)
    }

    async fn journal(&self, instance_id: &str, event: Value) -> Result<bool> {
        let append = self
            .hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        Ok(append.replayed)
    }

    /// Append `event` at the next seq; returns the seq it got.
    async fn journal_seq(&self, instance_id: &str, event: Value) -> Result<i64> {
        let append = self
            .hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        anyhow::ensure!(!append.replayed, "a fresh append");
        Ok(append.record.seq)
    }

    async fn journal_at(&self, instance_id: &str, seq: i64, event: Value) -> Result<bool> {
        let append = self
            .hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), Some(seq), event)
            .await?;
        Ok(append.replayed)
    }

    /// Every fact in `lineage`'s inbox, read as `token`.
    async fn inbox(&self, token: &str, lineage: &str) -> Result<Vec<Value>> {
        let (status, body) = self
            .request(
                reqwest::Method::GET,
                token,
                &format!("/v1/lineages/{lineage}/inbox?limit=200"),
                None,
            )
            .await?;
        anyhow::ensure!(status == 200, "GET inbox {lineage}: {status} {body}");
        Ok(body["items"].as_array().context("items")?.clone())
    }

    async fn inbox_status(&self, token: &str, lineage: &str) -> Result<u16> {
        Ok(self
            .request(
                reqwest::Method::GET,
                token,
                &format!("/v1/lineages/{lineage}/inbox"),
                None,
            )
            .await?
            .0)
    }

    async fn patch_inbox(&self, token: &str, lineage: &str, body: Value) -> Result<(u16, Value)> {
        self.request(
            reqwest::Method::PATCH,
            token,
            &format!("/v1/lineages/{lineage}/inbox"),
            Some(body),
        )
        .await
    }
}

/// Facts of `kind` about `subject` in an inbox page.
fn of_kind<'a>(items: &'a [Value], kind: &str, subject: &str) -> Vec<&'a Value> {
    items
        .iter()
        .filter(|fact| fact["kind"] == kind && fact["subjectInstanceId"] == subject)
        .collect()
}

// ─── journal events ─────────────────────────────────────────────────────────

const T0: &str = "2026-11-01T08:00:00.000Z";

/// `T0` plus `seconds`, in the journal's RFC3339 shape.
fn at(seconds: i64) -> String {
    let base = time::OffsetDateTime::parse(T0, &time::format_description::well_known::Rfc3339)
        .expect("T0");
    let t = base + time::Duration::seconds(seconds);
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

fn entity(state: &str, observed_at: &str) -> Value {
    json!({ "kind": "lifecycle", "observedAt": observed_at,
            "payload": { "type": "entity", "entityType": "instance", "state": state } })
}

fn activity(value: &str, observed_at: &str) -> Value {
    json!({ "kind": "lifecycle", "observedAt": observed_at, "payload": {
        "type": "native", "topic": "session", "nativeName": "session",
        "status": { "state": "known", "value": value },
        "relatedIds": {}, "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn question(id: &str, observed_at: &str) -> Value {
    json!({ "kind": "interaction.requested", "observedAt": observed_at, "payload": { "interaction": {
        "id": id, "kind": "question", "state": "pending", "blocking": true,
        "answerable": true, "carrier": "harness-hook",
        "request": { "kind": "question", "title": "Which way?", "options": [] } } } })
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

// ─── cases ──────────────────────────────────────────────────────────────────

/// The basic path: a dispatched worker's lifecycle, interaction and turn
/// result become facts in the dispatcher's inbox, readable by the owner and
/// by the lineage itself, and by nobody else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatched_child_yields_facts_its_lineage_reads() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-basic").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    let worker_token = ctx.agent_token(&worker).await?;

    ctx.journal(&worker, entity("running", &at(0))).await?;
    ctx.journal(&worker, activity("working", &at(1))).await?;
    ctx.journal(&worker, question("int_basic_1", &at(2)))
        .await?;
    ctx.journal(&worker, activity("idle", &at(3))).await?;

    let items = ctx.inbox(&ctx.human, &seat).await?;
    let kinds: Vec<&str> = items.iter().filter_map(|f| f["kind"].as_str()).collect();
    assert_eq!(kinds, ["started", "interaction", "turn-done"], "{items:#?}");
    for fact in &items {
        assert_eq!(fact["lineageId"], seat.as_str());
        assert_eq!(fact["subjectInstanceId"], worker.as_str());
        assert!(fact["batchId"].is_null(), "no batch before delivery");
    }
    let interaction = &items[1];
    assert_eq!(interaction["payload"]["interactionId"], "int_basic_1");
    assert_eq!(interaction["payload"]["interactionKind"], "question");
    // D-051 is off in the test Hub: the question pages the owner instead.
    assert_eq!(interaction["payload"]["routedToParent"], false);
    assert_eq!(
        interaction["dedupeKey"],
        format!("interaction:{worker}/int_basic_1:1:{}", at(2)),
        "kind:subject:run:stateEnteredAt, journal time"
    );
    assert_eq!(items[2]["runGeneration"], 1);
    assert_eq!(items[2]["payload"]["outcome"], "done");

    // The lineage reads its own inbox; the paging cursor is exclusive.
    let own = ctx.inbox(&seat_token, &seat).await?;
    assert_eq!(own, items);
    let (status, page) = ctx
        .request(
            reqwest::Method::GET,
            &seat_token,
            &format!("/v1/lineages/{seat}/inbox?after={}&limit=1", items[0]["id"]),
            None,
        )
        .await?;
    assert_eq!(status, 200);
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["items"][0], items[1]);
    assert_eq!(page["nextAfter"], items[1]["id"]);
    assert_eq!(page["more"], true);

    // Nobody else: the worker cannot read its dispatcher's inbox, a Bot is
    // refused outright, and an unknown lineage is 404 for the owner.
    assert_eq!(ctx.inbox_status(&worker_token, &seat).await?, 403);
    let bot = ctx
        .post(
            "unused-bearer",
            "/v1/login",
            json!({ "bootstrapToken": ctx.hub.bootstrap_token,
                    "deviceName": "inbox-bot", "deviceKind": "bot" }),
        )
        .await?["token"]
        .as_str()
        .context("bot token")?
        .to_string();
    assert_eq!(ctx.inbox_status(&bot, &seat).await?, 403);
    assert_eq!(ctx.inbox_status(&ctx.human, "ins_nobody").await?, 404);
    // The worker's own inbox is empty: nothing it dispatched did anything.
    assert!(ctx.inbox(&worker_token, &worker).await?.is_empty());
    Ok(())
}

/// §10.1 item 2: the cap counts per `(subscriber, instance, kind)` in a
/// rolling 24 h window, survives a Hub restart, and the next recorded fact
/// carries the count. Hitting it limits nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_occurrence_cap_is_per_subject_and_kind_rolls_and_survives_a_restart() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-cap").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    let sibling = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;

    // Seven questions, one a minute: the seventh is over the default cap.
    for n in 1..=7 {
        ctx.journal(&worker, question(&format!("int_cap_{n}"), &at(60 * n)))
            .await?;
    }
    let items = ctx.inbox(&ctx.human, &seat).await?;
    let asked = of_kind(&items, "interaction", &worker);
    assert_eq!(asked.len(), 6, "six recorded, the seventh capped");
    assert_eq!(asked[5]["payload"]["occurrence"]["count"], 6);
    assert_eq!(asked[5]["payload"]["occurrence"]["cap"], 6);

    // The cap is per subject and per kind: a sibling's question and the same
    // worker's turn result are still recorded.
    ctx.journal(&sibling, question("int_cap_sibling", &at(500)))
        .await?;
    ctx.journal(&worker, activity("idle", &at(510))).await?;

    // Restart: the count persists, so an eighth question is still capped.
    let ctx = ctx.restart().await?;
    ctx.journal(&worker, question("int_cap_8", &at(600)))
        .await?;
    let items = ctx.inbox(&ctx.human, &seat).await?;
    assert_eq!(of_kind(&items, "interaction", &worker).len(), 6);
    assert_eq!(of_kind(&items, "interaction", &sibling).len(), 1);
    assert_eq!(of_kind(&items, "turn-done", &worker).len(), 1);

    // Rolling: once the first question (T0+60 s) is more than 24 h old, one
    // more fits; it carries the count of everything still in the window.
    ctx.journal(&worker, question("int_cap_late", &at(24 * 3600 + 90)))
        .await?;
    let items = ctx.inbox(&ctx.human, &seat).await?;
    let asked = of_kind(&items, "interaction", &worker);
    assert_eq!(asked.len(), 7);
    let occurrence = &asked[6]["payload"]["occurrence"];
    // In (T0+90 s, T0+24 h+90 s]: questions 2..=8 (seven) plus this one.
    assert_eq!(occurrence["count"], 8, "{occurrence}");
    assert_eq!(occurrence["capped"], 2, "{occurrence}");
    // And the window is full again.
    ctx.journal(&worker, question("int_cap_later", &at(24 * 3600 + 100)))
        .await?;
    let items = ctx.inbox(&ctx.human, &seat).await?;
    assert_eq!(of_kind(&items, "interaction", &worker).len(), 7);
    Ok(())
}

/// §10.1 item 3: a structured carrier's result names its turn, so a late
/// result of an earlier run is recorded as that run's outcome and leaves the
/// newer run open; the newer run's own end is still recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_result_of_an_earlier_run_does_not_settle_the_newer_run() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-runs").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;

    ctx.journal(&worker, turn_started("cmd_run_1", &at(10)))
        .await?;
    ctx.journal(
        &worker,
        turn_result("turn_done", Some("cmd_run_1"), true, None, &at(20)),
    )
    .await?;
    ctx.journal(&worker, turn_started("cmd_run_2", &at(30)))
        .await?;
    // A late result for run 1 arrives while run 2 works (not settling).
    ctx.journal(
        &worker,
        turn_result("error", Some("cmd_run_1"), false, Some("late 429"), &at(40)),
    )
    .await?;

    let items = ctx.inbox(&ctx.human, &seat).await?;
    let ends: Vec<(&str, i64, bool)> = items
        .iter()
        .filter(|f| matches!(f["kind"].as_str(), Some("turn-done" | "turn-failed")))
        .map(|f| {
            (
                f["kind"].as_str().unwrap(),
                f["runGeneration"].as_i64().unwrap(),
                f["payload"]["late"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ends,
        [("turn-done", 1, false), ("turn-failed", 1, true)],
        "{items:#?}"
    );
    let late = items.iter().find(|f| f["kind"] == "turn-failed").unwrap();
    assert_eq!(late["payload"]["currentRun"], 2);
    assert_eq!(late["payload"]["error"], "late 429");
    assert!(
        late["dedupeKey"]
            .as_str()
            .unwrap()
            .starts_with(&format!("turn-failed:{worker}:1:")),
        "the run is in the key: {late}"
    );
    // Run 2 is still working: the late result settled nothing.
    let (_, instance) = ctx
        .request(
            reqwest::Method::GET,
            &ctx.human,
            &format!("/v1/instances/{worker}"),
            None,
        )
        .await?;
    let activity = instance
        .pointer("/instance/activity")
        .or_else(|| instance.get("activity"))
        .cloned();
    assert_eq!(activity, Some(json!("working")), "{instance}");

    // Run 2 ends: its own fact, run 2, not swallowed by run 1's.
    ctx.journal(
        &worker,
        turn_result("turn_done", Some("cmd_run_2"), true, None, &at(50)),
    )
    .await?;
    let items = ctx.inbox(&ctx.human, &seat).await?;
    let run_two: Vec<_> = items
        .iter()
        .filter(|f| f["kind"] == "turn-done" && f["runGeneration"] == 2)
        .collect();
    assert_eq!(run_two.len(), 1, "{items:#?}");
    assert_eq!(run_two[0]["payload"]["late"], false);
    Ok(())
}

/// §10.1 item 4: a lineage never receives facts about its own chapters. A
/// seat's own turns, questions and exit land in no inbox (it has no parent
/// lineage), while its worker's land in the seat's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_facts_about_a_lineages_own_chapters() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-own").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    let worker_token = ctx.agent_token(&worker).await?;

    for subject in [&seat, &worker] {
        ctx.journal(subject, entity("running", &at(0))).await?;
        ctx.journal(subject, activity("working", &at(1))).await?;
        ctx.journal(subject, question(&format!("int_own_{subject}"), &at(2)))
            .await?;
        ctx.journal(subject, activity("idle", &at(3))).await?;
        ctx.journal(subject, entity("exited", &at(4))).await?;
    }
    let seat_inbox = ctx.inbox(&seat_token, &seat).await?;
    assert!(
        seat_inbox
            .iter()
            .all(|fact| fact["subjectInstanceId"] == worker.as_str()),
        "the seat's inbox holds only its worker's facts: {seat_inbox:#?}"
    );
    let kinds: Vec<&str> = seat_inbox
        .iter()
        .filter_map(|f| f["kind"].as_str())
        .collect();
    assert_eq!(kinds, ["started", "interaction", "turn-done", "exited"]);
    assert!(ctx.inbox(&worker_token, &worker).await?.is_empty());
    Ok(())
}

/// §10.1 item 1: the dedupe key is built from journal time, so a replayed
/// append adds nothing and a Hub restart changes no key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dedupe_key_is_stable_across_a_restart_and_a_replayed_append() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-keys").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    let subject_facts = |items: Vec<Value>| -> Vec<Value> {
        items
            .into_iter()
            .filter(|fact| fact["subjectInstanceId"] == worker.as_str())
            .collect()
    };
    let started = ctx.journal_seq(&worker, entity("running", &at(0))).await?;
    let asked = ctx
        .journal_seq(&worker, question("int_key_1", &at(5)))
        .await?;
    let before = ctx.inbox(&ctx.human, &seat).await?;
    assert_eq!(before.len(), 2);

    // The Node replays the same rows (a lost acknowledgement).
    assert!(
        ctx.journal_at(&worker, started, entity("running", &at(0)))
            .await?
    );
    assert!(
        ctx.journal_at(&worker, asked, question("int_key_1", &at(5)))
            .await?
    );
    assert_eq!(ctx.inbox(&ctx.human, &seat).await?, before);

    // After a restart, the replay is still nothing new, and the keys and
    // rows read back unchanged.
    let ctx = ctx.restart().await?;
    assert!(
        ctx.journal_at(&worker, asked, question("int_key_1", &at(5)))
            .await?
    );
    let after = subject_facts(ctx.inbox(&ctx.human, &seat).await?);
    assert_eq!(after, before);
    assert_eq!(
        after[0]["dedupeKey"],
        format!("started:{worker}:0:{}", at(0))
    );
    assert_eq!(after[0]["stateEnteredAt"], at(0));

    // A re-sent request for an interaction the Hub already holds (a new row,
    // a later journal time) is not a second fact either.
    assert!(!ctx.journal(&worker, question("int_key_1", &at(9))).await?);
    assert_eq!(subject_facts(ctx.inbox(&ctx.human, &seat).await?), before);
    Ok(())
}

/// §10 Defaults: continuity lineages and agent-created coordinators receive
/// facts; a Human-created instance without continuity only after its creator
/// opts in; a muted kind is never recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_opt_in_and_mute_matrix() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-matrix").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;

    // Human-created, no continuity: its resume child's facts are not kept.
    let owner_session = ctx.plain_instance(json!({ "origin": "human" })).await?;
    let owner_child = ctx
        .plain_instance(json!({ "origin": "human", "parentInstanceId": owner_session }))
        .await?;
    // Agent-created (no grants), with a child of its own.
    let agent_made = ctx.plain_instance(json!({ "origin": "agent" })).await?;
    let agent_child = ctx
        .plain_instance(json!({ "origin": "agent", "parentInstanceId": agent_made }))
        .await?;

    for subject in [&worker, &owner_child, &agent_child] {
        ctx.journal(subject, entity("running", &at(0))).await?;
        ctx.journal(subject, question(&format!("int_mx_{subject}"), &at(1)))
            .await?;
    }
    assert_eq!(ctx.inbox(&ctx.human, &seat).await?.len(), 2, "continuity");
    assert_eq!(
        ctx.inbox(&ctx.human, &agent_made).await?.len(),
        2,
        "agent-created"
    );
    assert!(
        ctx.inbox(&ctx.human, &owner_session).await?.is_empty(),
        "a Human-created plain instance gets none by default"
    );

    // Only the owner opts in; the lineage itself cannot.
    let owner_token = ctx.agent_token(&owner_session).await?;
    let (status, _) = ctx
        .patch_inbox(&owner_token, &owner_session, json!({ "optIn": true }))
        .await?;
    assert_eq!(status, 403);
    let (status, settings) = ctx
        .patch_inbox(&ctx.human, &owner_session, json!({ "optIn": true }))
        .await?;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings, json!({ "optIn": true, "mutedKinds": [] }));
    ctx.journal(&owner_child, question("int_mx_after", &at(2)))
        .await?;
    let opted = ctx.inbox(&ctx.human, &owner_session).await?;
    assert_eq!(opted.len(), 1);
    assert_eq!(opted[0]["payload"]["interactionId"], "int_mx_after");

    // The lineage mutes a kind for itself: none of that kind, others still.
    let (status, settings) = ctx
        .patch_inbox(&seat_token, &seat, json!({ "mutedKinds": ["interaction"] }))
        .await?;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["mutedKinds"], json!(["interaction"]));
    ctx.journal(&worker, question("int_mx_muted", &at(3)))
        .await?;
    ctx.journal(&worker, activity("idle", &at(4))).await?;
    let items = ctx.inbox(&seat_token, &seat).await?;
    assert!(
        !items
            .iter()
            .any(|f| f["payload"]["interactionId"] == "int_mx_muted"),
        "{items:#?}"
    );
    assert_eq!(items.last().unwrap()["kind"], "turn-done");
    // A lineage reads and changes only its own settings.
    let (status, _) = ctx
        .patch_inbox(&seat_token, &agent_made, json!({ "mutedKinds": [] }))
        .await?;
    assert_eq!(status, 403);
    // Unmuted, the kind is recorded again.
    ctx.patch_inbox(&seat_token, &seat, json!({ "mutedKinds": [] }))
        .await?;
    ctx.journal(&worker, question("int_mx_unmuted", &at(5)))
        .await?;
    let items = ctx.inbox(&seat_token, &seat).await?;
    assert_eq!(
        items.last().unwrap()["payload"]["interactionId"],
        "int_mx_unmuted"
    );
    Ok(())
}

/// F supersedes a paused chapter's pending interactions (D-057 §7.2 step
/// 8): a fact for the chapter's parent lineage, never for its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn superseded_interactions_are_facts_for_the_parent_lineage() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-superseded").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    // A continuity child: it holds a grant.
    let created = ctx
        .post(
            &seat_token,
            "/v1/instances",
            json!({ "role": "worker", "grants": ["spend"], "projectId": project,
                    "prompt": "work", "driver": "claude-pty" }),
        )
        .await?;
    let child = created["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_string();
    ctx.journal(&child, entity("running", &at(0))).await?;
    ctx.journal(&child, question("int_sup_1", &at(1))).await?;

    let (status, body) = ctx
        .request(
            reqwest::Method::POST,
            &ctx.human,
            &format!("/v1/instances/{child}/commands"),
            Some(json!({"operation": "instance.close", "payload": {}})),
        )
        .await?;
    assert!((200..300).contains(&status), "pause: {status} {body}");

    let items = ctx.inbox(&seat_token, &seat).await?;
    let superseded = of_kind(&items, "interaction-superseded", &child);
    assert_eq!(superseded.len(), 1, "{items:#?}");
    assert_eq!(superseded[0]["payload"]["interactionId"], "int_sup_1");
    assert_eq!(superseded[0]["payload"]["interactionKind"], "question");
    assert_eq!(superseded[0]["payload"]["supersededBy"], child.as_str());
    assert!(
        ctx.inbox(&ctx.human, &child).await?.is_empty(),
        "never in the paused lineage's own inbox"
    );
    Ok(())
}

/// The scheduler's clock `minutes` from now, for the inbox tick hook.
fn minutes_from_now(minutes: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::minutes(minutes);
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("format")
}

/// Hosts in scope going offline and online, observed on the link. A drop is
/// announced once the link stays down for the host-lost grace (10 min in
/// the test Hub), counted from no earlier than the Hub's own start; the
/// hello after it is the online fact. A blip, the boot bookkeeping that
/// marks every host offline and the Hub's own restart announce nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_link_changes_are_facts_for_lineages_in_scope() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-hosts").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let token = ctx
        .node
        .as_ref()
        .and_then(|node| node.node_token.clone())
        .context("node token")?;
    let host_facts = |items: &[Value]| -> Vec<String> {
        items
            .iter()
            .filter(|f| f["payload"]["hostId"].is_string())
            .map(|f| f["kind"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(host_facts(&ctx.inbox(&seat_token, &seat).await?).is_empty());

    // A blip: down and back inside the grace.
    ctx.disconnect().await?;
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(5)).await?, 0);
    ctx.node = Some(connect_node(&ctx.hub, &ctx.host, &ctx.workspace, &token).await?);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(11)).await?, 0);
    assert!(host_facts(&ctx.inbox(&seat_token, &seat).await?).is_empty());

    // Down past the grace: one offline fact, however often the tick runs.
    ctx.disconnect().await?;
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(11)).await?, 1);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(12)).await?, 0);
    assert_eq!(
        host_facts(&ctx.inbox(&seat_token, &seat).await?),
        ["host-offline"]
    );
    // Back: the online fact.
    ctx.node = Some(connect_node(&ctx.hub, &ctx.host, &ctx.workspace, &token).await?);
    let items = ctx.inbox(&seat_token, &seat).await?;
    assert_eq!(host_facts(&items), ["host-offline", "host-online"]);
    assert_eq!(
        items.last().unwrap()["payload"]["hostId"],
        ctx.host.as_str()
    );

    // The Hub restarts: its own shutdown drops the link and the boot marks
    // every host offline; the Node returns inside the grace. Nothing.
    let mut ctx = ctx.restart().await?;
    ctx.node = Some(connect_node(&ctx.hub, &ctx.host, &ctx.workspace, &token).await?);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(30)).await?, 0);
    assert_eq!(
        host_facts(&ctx.inbox(&seat_token, &seat).await?),
        ["host-offline", "host-online"]
    );

    // The Hub restarts and the Node does not come back: the grace runs from
    // the new process's start, then the offline fact follows.
    let ctx = ctx.restart().await?;
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(5)).await?, 0);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(11)).await?, 1);
    assert_eq!(
        host_facts(&ctx.inbox(&seat_token, &seat).await?),
        ["host-offline", "host-online", "host-offline"]
    );
    Ok(())
}

// ─── hook faults (r2 item 2) ────────────────────────────────────────────────

/// "A fact must never harm the append" is structural: the inbox fold runs in
/// its own savepoint, so a failing fold rolls back to it, logs, and the
/// journal row still commits; the writer keeps serving afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_inbox_hook_rolls_back_and_leaves_the_append_committed() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-hook-fault").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;

    remuda_hub::inbox_test_support::arm_fold_failure(&worker);
    // The append whose fact would fail is still durable.
    assert!(!ctx.journal(&worker, entity("running", &at(0))).await?);
    let committed: i64 = ctx.db()?.query_row(
        "SELECT COUNT(*) FROM journal
          WHERE instance_id = ?1 AND json_extract(payload_json, '$.payload.state') = 'running'",
        rusqlite::params![worker],
        |row| row.get(0),
    )?;
    assert_eq!(committed, 1, "the journal row committed");
    assert!(
        ctx.inbox(&ctx.human, &seat).await?.is_empty(),
        "the failed fold recorded nothing"
    );

    // The writer is usable: the next append records its fact.
    ctx.journal(&worker, question("int_after_hook_fault", &at(1)))
        .await?;
    let items = ctx.inbox(&ctx.human, &seat).await?;
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0]["payload"]["interactionId"], "int_after_hook_fault");
    Ok(())
}

/// §10.1 item 2, the subscriber half of the key: two seats in one project,
/// each with its own worker, each with its own cap in the same rolling
/// window. Exhausting one leaves the other recording.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_subscriber_has_its_own_occurrence_cap() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-cap-two").await?;
    // One dispatch holder per project is the default; this test needs two.
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            Some(json!({ "policy": { "configurable": { "allowMultipleDispatchers": true } } })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (seat_a, token_a) = ctx.coordinator(&project).await?;
    let (seat_b, token_b) = ctx.coordinator(&project).await?;
    let worker_a = ctx.worker(&token_a, &project).await?;
    let worker_b = ctx.worker(&token_b, &project).await?;

    for (worker, prefix) in [(&worker_a, "a"), (&worker_b, "b")] {
        ctx.journal(worker, entity("running", &at(0))).await?;
        for n in 1..=7 {
            ctx.journal(worker, question(&format!("int_{prefix}_{n}"), &at(60 * n)))
                .await?;
        }
    }

    let a_items = ctx.inbox(&token_a, &seat_a).await?;
    let b_items = ctx.inbox(&token_b, &seat_b).await?;
    assert_eq!(
        of_kind(&a_items, "interaction", &worker_a).len(),
        6,
        "{a_items:#?}"
    );
    assert_eq!(
        of_kind(&b_items, "interaction", &worker_b).len(),
        6,
        "{b_items:#?}"
    );
    // Neither seat hears about the other's worker.
    assert!(of_kind(&a_items, "interaction", &worker_b).is_empty());
    assert!(of_kind(&b_items, "interaction", &worker_a).is_empty());
    // Both caps counted only their own occurrences.
    let last = of_kind(&a_items, "interaction", &worker_a);
    assert_eq!(last[5]["payload"]["occurrence"]["count"], 6);
    let last = of_kind(&b_items, "interaction", &worker_b);
    assert_eq!(last[5]["payload"]["occurrence"]["count"], 6);

    // An eighth for each, still inside the same window: both capped, and
    // both counts keep rising independently.
    ctx.journal(&worker_a, question("int_a_8", &at(600)))
        .await?;
    ctx.journal(&worker_b, question("int_b_8", &at(600)))
        .await?;
    assert_eq!(
        of_kind(
            &ctx.inbox(&token_a, &seat_a).await?,
            "interaction",
            &worker_a
        )
        .len(),
        6
    );
    assert_eq!(
        of_kind(
            &ctx.inbox(&token_b, &seat_b).await?,
            "interaction",
            &worker_b
        )
        .len(),
        6
    );
    let db = ctx.db()?;
    for (seat, worker) in [(&seat_a, &worker_a), (&seat_b, &worker_b)] {
        let (total, recorded): (i64, i64) = db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(recorded), 0) FROM inbox_occurrences
              WHERE subscriber_lineage_id = ?1 AND subject = ?2 AND kind = 'interaction'",
            rusqlite::params![seat, worker],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!((total, recorded), (8, 6), "{seat}/{worker}");
    }
    Ok(())
}

/// r3 item 2: the fold fails AFTER it has written its first fact. The
/// savepoint must undo that partial write — no `inbox_facts` row and no
/// `inbox_occurrences` row for the event — while the journal row and the
/// projections it drove still commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_failing_after_a_recorded_fact_leaves_no_partial_write() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-hook-partial").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;

    remuda_hub::inbox_test_support::arm_fold_failure_after_record(&worker);
    // `running` records a started fact first; the seam fails right after it.
    assert!(!ctx.journal(&worker, entity("running", &at(0))).await?);

    let db = ctx.db()?;
    let journaled: i64 = db.query_row(
        "SELECT COUNT(*) FROM journal
          WHERE instance_id = ?1 AND json_extract(payload_json, '$.payload.state') = 'running'",
        rusqlite::params![worker],
        |row| row.get(0),
    )?;
    assert_eq!(journaled, 1, "the journal row committed");
    let lifecycle: String = db.query_row(
        "SELECT lifecycle FROM instances WHERE id = ?1",
        rusqlite::params![worker],
        |row| row.get(0),
    )?;
    assert_eq!(lifecycle, "running", "the append's projections committed");
    let facts: i64 = db.query_row(
        "SELECT COUNT(*) FROM inbox_facts WHERE subject_instance_id = ?1",
        rusqlite::params![worker],
        |row| row.get(0),
    )?;
    let occurrences: i64 = db.query_row(
        "SELECT COUNT(*) FROM inbox_occurrences WHERE subject = ?1",
        rusqlite::params![worker],
        |row| row.get(0),
    )?;
    assert_eq!(
        (facts, occurrences),
        (0, 0),
        "the started fact and its occurrence were rolled back"
    );
    drop(db);

    // The writer is usable and the next fact is recorded normally.
    ctx.journal(&worker, question("int_after_partial", &at(1)))
        .await?;
    let items = ctx.inbox(&ctx.human, &seat).await?;
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0]["kind"], "interaction");
    assert_eq!(items[0]["payload"]["occurrence"]["count"], 1);
    Ok(())
}

// ─── stalls and wall budgets (§10.1 item 8) ─────────────────────────────────

fn tool_call(id: &str) -> Value {
    json!({ "kind": "tool_call", "payload": { "operation": "open", "toolCallId": id,
        "toolName": { "state": "known", "value": "Bash" },
        "input": { "state": "known", "value": { "command": "cargo test --workspace" } },
        "state": "proposed" } })
}

fn tool_result(id: &str) -> Value {
    json!({ "kind": "tool_result", "payload": { "toolCallId": id, "stage": "final",
        "outcome": "succeeded" } })
}

fn stalls(items: &[Value]) -> Vec<&Value> {
    items.iter().filter(|f| f["kind"] == "stalled").collect()
}

/// No stall fact while a tool call is in flight, however long; once its
/// result arrives the ordinary quiet window counts from the result, and one
/// quiet period yields one fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_in_flight_yields_no_stall_fact_and_the_window_restarts_at_its_result() -> Result<()>
{
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-stall").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;
    ctx.journal(&worker, activity("working", &at(1))).await?;
    ctx.journal(&worker, tool_call("tc_long")).await?;

    // Two hours in flight, past any quiet window: nothing.
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(120)).await?, 0);
    assert!(stalls(&ctx.inbox(&seat_token, &seat).await?).is_empty());

    // The result arrives; the quiet window counts from it.
    // The project's own quiet window (a new project stores one).
    let (_, doc) = ctx
        .request(
            reqwest::Method::GET,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            None,
        )
        .await?;
    let window = doc
        .pointer("/policy/configurable/stallThresholdMins")
        .or_else(|| doc.pointer("/project/policy/configurable/stallThresholdMins"))
        .and_then(Value::as_i64)
        .context("stallThresholdMins")?;
    assert!(window < 120, "the in-flight wait above outlasted it");
    ctx.journal(&worker, tool_result("tc_long")).await?;
    assert_eq!(
        ctx.hub
            .test_inbox_tick(&minutes_from_now(window - 1))
            .await?,
        0
    );
    assert_eq!(
        ctx.hub
            .test_inbox_tick(&minutes_from_now(window + 1))
            .await?,
        1
    );
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(240)).await?, 0);
    let items = ctx.inbox(&seat_token, &seat).await?;
    let stalled = stalls(&items);
    assert_eq!(stalled.len(), 1, "{items:#?}");
    assert_eq!(stalled[0]["subjectInstanceId"], worker.as_str());
    assert_eq!(stalled[0]["payload"]["cause"], "quiet");
    assert_eq!(stalled[0]["payload"]["thresholdMins"], window);
    Ok(())
}

/// With a finite `stallToolThresholdMins`, a tool call in flight longer than
/// that yields exactly one stall fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_tool_threshold_yields_exactly_one_stall_fact() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-stall-tool").await?;
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            Some(json!({ "policy": { "configurable": { "stallToolThresholdMins": 15 } } })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;
    ctx.journal(&worker, activity("working", &at(1))).await?;
    ctx.journal(&worker, tool_call("tc_hung")).await?;

    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(14)).await?, 0);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(16)).await?, 1);
    for minutes in [30, 60, 600] {
        assert_eq!(
            ctx.hub.test_inbox_tick(&minutes_from_now(minutes)).await?,
            0
        );
    }
    let items = ctx.inbox(&seat_token, &seat).await?;
    let stalled = stalls(&items);
    assert_eq!(stalled.len(), 1, "{items:#?}");
    assert_eq!(stalled[0]["payload"]["cause"], "tool-in-flight");
    assert_eq!(stalled[0]["payload"]["toolCallId"], "tc_hung");
    assert_eq!(stalled[0]["payload"]["thresholdMins"], 15);
    Ok(())
}

/// r3 item 1: a tool-stall fact is keyed by the instance, the call and the
/// time it opened, never by the run generation. A root `turn_started` while
/// the call is still tracked (its result never journaled, e.g. across a
/// host blip) bumps the run; it must not turn the same call into a second
/// stall fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_run_does_not_rekey_a_tool_stall() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-stall-run").await?;
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            // A year-long quiet window: only the tool window can yield here.
            Some(json!({ "policy": { "configurable": {
                "stallToolThresholdMins": 15, "stallThresholdMins": 525_600 } } })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;
    ctx.journal(&worker, turn_started("cmd_stall_run_1", &at(1)))
        .await?;
    ctx.journal(&worker, tool_call("tc_across_runs")).await?;

    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(16)).await?, 1);
    // A new root turn starts; the call's result was never journaled.
    ctx.journal(&worker, turn_started("cmd_stall_run_2", &at(2)))
        .await?;
    for minutes in [17, 60, 600] {
        assert_eq!(
            ctx.hub.test_inbox_tick(&minutes_from_now(minutes)).await?,
            0
        );
    }
    let items = ctx.inbox(&seat_token, &seat).await?;
    let stalled = stalls(&items);
    assert_eq!(stalled.len(), 1, "one call, one stall fact: {items:#?}");
    assert_eq!(stalled[0]["payload"]["toolCallId"], "tc_across_runs");
    assert_eq!(stalled[0]["runGeneration"], 1);
    Ok(())
}

/// A task's `maxWallMins` elapsing yields one budget-expired fact for the
/// dispatcher; the worker keeps running and nothing is sent to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_budget_yields_one_fact_and_kills_nothing() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-budget").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let task = ctx
        .post(
            &ctx.human,
            "/v1/tasks",
            json!({ "projectId": project, "title": "long", "intent": "a long task",
                    "budget": { "maxWallMins": 45 } }),
        )
        .await?;
    let task_id = task["id"]
        .as_str()
        .or_else(|| task["task"]["id"].as_str())
        .context("task id")?
        .to_string();
    let created = ctx
        .post(
            &seat_token,
            "/v1/instances",
            json!({ "role": "worker", "projectId": project, "prompt": "work",
                    "driver": "claude-pty", "taskId": task_id }),
        )
        .await?;
    let worker = created["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_string();
    ctx.journal(&worker, entity("running", &at(0))).await?;
    let (_, before) = ctx
        .request(
            reqwest::Method::GET,
            &ctx.human,
            &format!("/v1/instances/{worker}/commands"),
            None,
        )
        .await?;

    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(44)).await?, 0);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(46)).await?, 1);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(240)).await?, 0);
    let items = ctx.inbox(&seat_token, &seat).await?;
    let expired: Vec<_> = items
        .iter()
        .filter(|f| f["kind"] == "budget-expired")
        .collect();
    assert_eq!(expired.len(), 1, "{items:#?}");
    assert_eq!(expired[0]["subjectInstanceId"], worker.as_str());
    assert_eq!(expired[0]["payload"]["taskId"], task_id.as_str());
    assert_eq!(expired[0]["payload"]["maxWallMins"], 45);
    assert!(
        expired[0]["dedupeKey"]
            .as_str()
            .unwrap()
            .starts_with(&format!("budget-expired:{task_id}:0:")),
        "{}",
        expired[0]
    );
    // Kills nothing: no command was queued for the worker, and it runs.
    let (_, after) = ctx
        .request(
            reqwest::Method::GET,
            &ctx.human,
            &format!("/v1/instances/{worker}/commands"),
            None,
        )
        .await?;
    assert_eq!(after, before);
    let (_, instance) = ctx
        .request(
            reqwest::Method::GET,
            &ctx.human,
            &format!("/v1/instances/{worker}"),
            None,
        )
        .await?;
    let lifecycle = instance
        .pointer("/instance/lifecycle")
        .or_else(|| instance.get("lifecycle"))
        .cloned();
    assert_eq!(lifecycle, Some(json!("running")), "{instance}");
    Ok(())
}

// ─── out-of-range windows (r2 item 1) ───────────────────────────────────────

/// The Hub's database, written directly: a value the routes would refuse.
fn store_minutes(
    db: &rusqlite::Connection,
    table: &str,
    id: &str,
    path: &str,
    mins: i64,
) -> Result<()> {
    let sql =
        format!("UPDATE {table} SET doc_json = json_set(doc_json, '${path}', ?1) WHERE id = ?2");
    db.execute(&sql, rusqlite::params![mins, id])?;
    Ok(())
}

/// A stored window no `Duration` or time arithmetic can hold must neither
/// panic the Hub's only writer thread nor wrap into a past time: the tick
/// skips that subject, logs a category, and the writer keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_range_window_is_skipped_and_never_panics_the_writer() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-overflow").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    // One busy child per branch: the quiet window, the tool window.
    let quiet = ctx.worker(&seat_token, &project).await?;
    let hung = ctx.worker(&seat_token, &project).await?;
    let task = ctx
        .post(
            &ctx.human,
            "/v1/tasks",
            json!({ "projectId": project, "title": "overflow", "intent": "overflow",
                    "budget": { "maxWallMins": 45 } }),
        )
        .await?;
    let task_id = task["id"]
        .as_str()
        .or_else(|| task["task"]["id"].as_str())
        .context("task id")?
        .to_string();
    let created = ctx
        .post(
            &seat_token,
            "/v1/instances",
            json!({ "role": "worker", "projectId": project, "prompt": "work",
                    "driver": "claude-pty", "taskId": task_id }),
        )
        .await?;
    let budgeted = created["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_string();
    for worker in [&quiet, &hung, &budgeted] {
        ctx.journal(worker, entity("running", &at(0))).await?;
        ctx.journal(worker, activity("working", &at(1))).await?;
    }
    ctx.journal(&hung, tool_call("tc_overflow")).await?;

    // The routes would refuse these; the database still holds them (an older
    // build, a hand edit, a restore).
    let db = ctx.db()?;
    store_minutes(
        &db,
        "projects",
        &project,
        ".policy.configurable.stallThresholdMins",
        i64::MAX,
    )?;
    store_minutes(
        &db,
        "projects",
        &project,
        ".policy.configurable.stallToolThresholdMins",
        i64::MAX,
    )?;
    store_minutes(&db, "tasks", &task_id, ".budget.maxWallMins", i64::MAX)?;
    drop(db);

    // A tick far enough ahead that every deadline has passed.
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(600)).await?, 0);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(1200)).await?, 0);

    // The writer still commits: a normal append and a settings write.
    ctx.journal(&quiet, question("int_after_overflow", &at(2)))
        .await?;
    let (status, settings) = ctx
        .patch_inbox(&seat_token, &seat, json!({ "mutedKinds": ["waiting"] }))
        .await?;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["mutedKinds"], json!(["waiting"]));

    // Nothing was recorded for the subjects whose window could not be used.
    let items = ctx.inbox(&ctx.human, &seat).await?;
    assert!(
        items.iter().all(|fact| fact["kind"] != "stalled"),
        "{items:#?}"
    );
    assert!(
        items.iter().all(|fact| fact["kind"] != "budget-expired"),
        "{items:#?}"
    );
    Ok(())
}

/// A host-lost grace too large for a `Duration` is clamped at load, not
/// subtracted blindly on the tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_range_host_grace_is_clamped_at_load() -> Result<()> {
    let ctx = Ctx::spawn_with(|config| config.host_lost_grace_ms = u64::MAX).await?;
    let project = ctx.project("inbox-grace-overflow").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(600)).await?, 0);
    // The writer still commits.
    let (status, settings) = ctx
        .patch_inbox(&seat_token, &seat, json!({ "mutedKinds": [] }))
        .await?;
    assert_eq!(status, 200, "{settings}");
    Ok(())
}

/// The write routes refuse a window the tick could not use with 400, so the
/// tick never sees one from a live caller: above a year or negative for
/// every window, and 0 for the tool window (absent is "never" there); the
/// two older fields keep accepting 0, their documented "default"/"none".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_windows_are_rejected_at_the_routes() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-route-limits").await?;
    for (field, zero) in [("stallThresholdMins", 200), ("stallToolThresholdMins", 400)] {
        for (mins, status) in [
            (i64::MAX, 400),
            (-1, 400),
            (525_601, 400),
            (0, zero),
            (45, 200),
            (525_600, 200),
        ] {
            let (got, body) = ctx
                .request(
                    reqwest::Method::PATCH,
                    &ctx.human,
                    &format!("/v1/projects/{project}"),
                    Some(json!({ "policy": { "configurable": { field: mins } } })),
                )
                .await?;
            assert_eq!(got, status, "{field}={mins}: {body}");
            if status == 400 {
                assert!(
                    body["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("minutes")),
                    "{field}={mins}: {body}"
                );
            }
        }
    }
    for (mins, status) in [
        (i64::MAX, 400),
        (-1, 400),
        (525_601, 400),
        (0, 200),
        (525_600, 200),
    ] {
        let (got, body) = ctx
            .request(
                reqwest::Method::POST,
                &ctx.human,
                "/v1/tasks",
                Some(
                    json!({ "projectId": project, "title": "window", "intent": "window",
                             "budget": { "maxWallMins": mins } }),
                ),
            )
            .await?;
        assert_eq!(got, status, "maxWallMins={mins}: {body}");
        if status == 400 {
            assert!(
                body["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("minutes")),
                "maxWallMins={mins}: {body}"
            );
        }
    }
    Ok(())
}

/// r2 item 3: a stall fact is keyed by what the stall is about — the tool
/// call and when it was opened, or the quiet period's start — never by the
/// threshold that measured it, so changing the project's window mid-stall
/// records no second fact for the same stall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_threshold_change_records_no_second_stall_fact() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-stall-rekey").await?;
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            Some(json!({ "policy": { "configurable": {
                "stallThresholdMins": 90, "stallToolThresholdMins": 15 } } })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let hung = ctx.worker(&seat_token, &project).await?;
    let quiet = ctx.worker(&seat_token, &project).await?;
    for worker in [&hung, &quiet] {
        ctx.journal(worker, entity("running", &at(0))).await?;
        ctx.journal(worker, activity("working", &at(1))).await?;
    }
    ctx.journal(&hung, tool_call("tc_rekey")).await?;

    // The tool call passes its 15 min window; the quiet child its 90.
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(16)).await?, 1);
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(95)).await?, 1);
    let items = ctx.inbox(&seat_token, &seat).await?;
    assert_eq!(stalls(&items).len(), 2, "{items:#?}");

    // Both windows change mid-stall (shorter now, both already passed).
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            Some(json!({ "policy": { "configurable": {
                "stallThresholdMins": 20, "stallToolThresholdMins": 5 } } })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    for minutes in [30, 120, 600] {
        assert_eq!(
            ctx.hub.test_inbox_tick(&minutes_from_now(minutes)).await?,
            0
        );
    }
    let items = ctx.inbox(&seat_token, &seat).await?;
    let stalls_now = stalls(&items);
    assert_eq!(
        stalls_now.len(),
        2,
        "one per stall, not per threshold: {items:#?}"
    );
    let tool = stalls_now
        .iter()
        .find(|fact| fact["subjectInstanceId"] == hung.as_str())
        .context("tool stall")?;
    assert_eq!(
        tool["payload"]["thresholdMins"], 15,
        "the payload keeps the threshold that measured it"
    );
    Ok(())
}

/// r2 item 4: a result naming a turn the tracker has forgotten (more than 32
/// root turns ago) is recorded as unattributed. It never settles the current
/// run and never clears a tool call that is still in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_for_a_forgotten_turn_is_unattributed_and_settles_nothing() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-old-turn").await?;
    let (status, body) = ctx
        .request(
            reqwest::Method::PATCH,
            &ctx.human,
            &format!("/v1/projects/{project}"),
            Some(json!({ "policy": { "configurable": { "stallToolThresholdMins": 1 } } })),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;
    // Forty root turns; the tracker remembers the last 32.
    for n in 1..=40 {
        let command = format!("cmd_turn_{n}");
        ctx.journal(&worker, turn_started(&command, &at(3 * n)))
            .await?;
        ctx.journal(
            &worker,
            turn_result("turn_done", Some(&command), false, None, &at(3 * n + 1)),
        )
        .await?;
    }
    ctx.journal(&worker, tool_call("tc_still_open")).await?;
    // A late error for the first turn, whose command the tracker dropped.
    ctx.journal(
        &worker,
        turn_result(
            "error",
            Some("cmd_turn_1"),
            false,
            Some("late 529"),
            &at(200),
        ),
    )
    .await?;

    let items = ctx.inbox(&seat_token, &seat).await?;
    let failed: Vec<_> = items
        .iter()
        .filter(|fact| fact["kind"] == "turn-failed")
        .collect();
    assert_eq!(failed.len(), 1, "{items:#?}");
    assert_eq!(failed[0]["payload"]["unattributed"], true, "{failed:#?}");
    assert_eq!(failed[0]["runGeneration"], 0, "no run is claimed");
    assert_eq!(failed[0]["payload"]["currentRun"], 40);
    assert!(
        failed[0]["dedupeKey"]
            .as_str()
            .unwrap()
            .starts_with(&format!("turn-failed:{worker}:0:")),
        "{}",
        failed[0]
    );

    // The tool call is still in flight: its own stall fact can still follow.
    assert_eq!(ctx.hub.test_inbox_tick(&minutes_from_now(3)).await?, 1);
    let items = ctx.inbox(&seat_token, &seat).await?;
    assert!(
        stalls(&items)
            .iter()
            .any(|fact| fact["payload"]["toolCallId"] == "tc_still_open"),
        "{items:#?}"
    );
    Ok(())
}

// ─── logs (§10.1 item 9) ────────────────────────────────────────────────────

/// Everything the inbox logs in this test binary, captured once.
fn inbox_logs() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> =
        std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "remuda_hub::inbox=trace",
            ))
            .with_ansi(false)
            .with_writer(move || Sink(sink.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one log capture");
        buffer
    })
    .clone()
}

struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The inbox logs the kind, the outcome, counts and error codes; never the
/// fact's text or anything a worker wrote, even when the fact carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_logs_carry_categories_and_counts_never_fact_text() -> Result<()> {
    let logs = inbox_logs();
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-logs").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    let marker = "worker-said-ZQ7XK2-confidential";
    ctx.journal(&worker, entity("running", &at(0))).await?;
    ctx.journal(&worker, turn_started("cmd_logs", &at(1)))
        .await?;
    let mut ask = question("int_logs_1", &at(2));
    ask["payload"]["interaction"]["request"]["title"] = json!(marker);
    ctx.journal(&worker, ask).await?;
    ctx.journal(
        &worker,
        turn_result("error", Some("cmd_logs"), true, Some(marker), &at(3)),
    )
    .await?;

    // The fact carries the worker's text (it is the agent's to read)…
    let items = ctx.inbox(&seat_token, &seat).await?;
    let failed = items
        .iter()
        .find(|f| f["kind"] == "turn-failed")
        .context("turn-failed fact")?;
    assert_eq!(failed["payload"]["error"], marker);

    // …the log does not. It names the kind, the outcome and the count.
    let text = String::from_utf8(logs.lock().expect("log buffer").clone())?;
    assert!(
        text.lines().any(|line| line.contains("inbox fact")
            && line.contains("kind=\"turn-failed\"")
            && line.contains("outcome=\"recorded\"")
            && line.contains("occurrences=1")),
        "{text}"
    );
    assert!(!text.contains(marker), "{text}");
    assert!(!text.contains(&worker), "no ids either: {text}");
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

/// Item 1: the inbox, its occurrence counts and its settings ride
/// `hub.sqlite`, so `scripts/hub-backup.sh` and a restore per hub-topology
/// §6.1 carry them: the restored Hub reads the same page and keeps capping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inbox_survives_backup_and_restore() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("inbox-backup").await?;
    let (seat, seat_token) = ctx.coordinator(&project).await?;
    let worker = ctx.worker(&seat_token, &project).await?;
    ctx.journal(&worker, entity("running", &at(0))).await?;
    for n in 1..=7 {
        ctx.journal(&worker, question(&format!("int_bk_{n}"), &at(60 * n)))
            .await?;
    }
    ctx.patch_inbox(&seat_token, &seat, json!({ "mutedKinds": ["waiting"] }))
        .await?;
    let mut ctx = ctx;
    ctx.disconnect().await?;
    let before = ctx
        .request(
            reqwest::Method::GET,
            &ctx.human,
            &format!("/v1/lineages/{seat}/inbox?limit=200"),
            None,
        )
        .await?
        .1;
    // started, six questions (the seventh capped).
    assert_eq!(before["items"].as_array().map(Vec::len), Some(7));

    let host = ctx.host.clone();
    let workspace = ctx.workspace.clone();
    let data = ctx.data_dir();
    let Ctx { dir, hub, .. } = ctx;
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
    run(Command::new("sha256sum")
        .arg("-c")
        .arg(bundle.with_extension("age.sha256").file_name().unwrap())
        .current_dir(&out))?;
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
    let restored_dir = tempfile::tempdir()?;
    let restored = restored_dir.path().join("hub");
    std::fs::create_dir_all(&restored)?;
    for member in manifest["members"].as_array().context("members")? {
        let rel = member["path"].as_str().context("path")?;
        let digest = run(Command::new("sha256sum").arg(rel).current_dir(&staged))?;
        assert_eq!(
            digest.split_whitespace().next(),
            member["sha256"].as_str(),
            "{rel}"
        );
        let target = restored.join(rel);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::copy(staged.join(rel), &target)?;
    }

    let hub = spawn(HubConfig::for_test(restored.clone())).await?;
    let human = hub.mint_device_token("inbox-restored").await?;
    let restored_ctx = Ctx {
        dir: restored_dir,
        hub,
        http: reqwest::Client::new(),
        human,
        host,
        workspace,
        node: None,
    };
    let after = restored_ctx
        .request(
            reqwest::Method::GET,
            &restored_ctx.human,
            &format!("/v1/lineages/{seat}/inbox?limit=200"),
            None,
        )
        .await?
        .1;
    assert_eq!(after, before, "rows, keys, counts and settings survive");

    // The restored Hub caps too: an eighth question is counted, not recorded.
    restored_ctx
        .journal(&worker, question("int_bk_8", &at(60 * 8)))
        .await?;
    let items = restored_ctx.inbox(&restored_ctx.human, &seat).await?;
    assert_eq!(
        of_kind(&items, "interaction", &worker).len(),
        6,
        "the cap and its count survive the restore"
    );
    drop(dir);
    Ok(())
}
