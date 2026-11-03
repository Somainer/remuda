//! cb-stall-tooluse (main-agent.md §10, §10.1 item 8, §10.5): tool-in-flight
//! state for every carrier, and the screen stall fallback.
//!
//! Each carrier's tool start and end are produced by that carrier's own
//! mapper over a recorded fixture (the claude stream-json mapper, the claude
//! transcript mapper, the claude hook live fold, the codex rollout adapter, the
//! grok session adapter), stripped to the `{kind, payload}` the Hub folds, and
//! appended to the in-process Hub's journal. Screens come from a fake Node
//! that answers `tty.screen` with scripted rows, or never answers (a dead
//! host). Journal times are the Hub's own; the tick and the sweep are driven
//! with a clock minutes ahead, so nothing waits on a wall-clock threshold.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Screen-read budget in the sweeps below: far longer than any answered read
/// takes, so a host that never answers is still waiting while the other
/// host's sweep finishes.
const SWEEP_TIMEOUT: Duration = Duration::from_secs(60);

// ─── fake Node with a scripted screen ───────────────────────────────────────

/// What the fake Node answers to `tty.screen`.
#[derive(Clone)]
enum Screen {
    Rows(Vec<String>),
    /// Never answers: the Hub's read waits out its timeout.
    Mute,
    /// Answers `supported: false`, as a Node does for a driver without
    /// `screen_read`.
    Unsupported,
}

struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    screen: Arc<Mutex<Screen>>,
    reads: Arc<AtomicUsize>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeNode {
    fn show(&self, rows: &[&str]) {
        *self.screen.lock().unwrap() =
            Screen::Rows(rows.iter().map(|row| (*row).to_owned()).collect());
    }

    fn mute(&self) {
        *self.screen.lock().unwrap() = Screen::Mute;
    }

    fn unsupported(&self) {
        *self.screen.lock().unwrap() = Screen::Unsupported;
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match ws.next().await {
        Some(Ok(Message::Text(text))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
    }
}

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
                    "herdr": {"version": "0.9.0", "socket": "/w/fake-herdr.sock"},
                    "workspaces": [{ "workspaceId": workspace, "hostId": host_id,
                                     "root": format!("/w/{workspace}") }],
                    "workspaceRevision": 1,
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = tokio::time::timeout(Duration::from_secs(8), recv_json(&mut node)).await??;
    anyhow::ensure!(hello.get("result").is_some(), "hello {hello}");
    let screen = Arc::new(Mutex::new(Screen::Rows(Vec::new())));
    let reads = Arc::new(AtomicUsize::new(0));
    let (task_screen, task_reads) = (Arc::clone(&screen), Arc::clone(&reads));
    let task = tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            let (Some(id), Some(method)) = (
                frame.get("id").cloned(),
                frame.get("method").and_then(Value::as_str),
            ) else {
                continue;
            };
            let result = if method == "tty.screen" {
                task_reads.fetch_add(1, Ordering::SeqCst);
                let answer = task_screen.lock().unwrap().clone();
                match answer {
                    Screen::Mute => continue,
                    Screen::Unsupported => json!({
                        "supported": false, "lifecycle": "running",
                        "reason": "this driver carries no readable screen" }),
                    Screen::Rows(lines) => json!({
                        "supported": true, "lifecycle": "running",
                        "source": "emulator", "lines": lines }),
                }
            } else {
                json!({ "accepted": true })
            };
            let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if node
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(FakeNode {
        task,
        screen,
        reads,
    })
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
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("stall-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = Self::enroll(&hub, &host, &workspace).await?;
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

    async fn enroll(hub: &remuda_hub::RunningHub, host: &str, workspace: &str) -> Result<FakeNode> {
        let enroll = hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?;
        connect_node(hub, host, workspace, &enroll).await
    }

    /// A second host with its own fake Node.
    async fn second_host(&self) -> Result<(String, FakeNode)> {
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = Self::enroll(&self.hub, &host, &workspace).await?;
        Ok((host, node))
    }

    fn node(&self) -> &FakeNode {
        self.node.as_ref().expect("node connected")
    }

    /// Stop the Hub and start a new one on the same data directory.
    async fn restart(self) -> Result<Ctx> {
        let Ctx {
            dir,
            hub,
            human,
            host,
            workspace,
            node,
            ..
        } = self;
        drop(node);
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

    fn db(&self) -> Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.data_dir().join("hub.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
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
            .request(method, format!("http://{}{path}", self.hub.addr))
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

    /// A project on the first host with `configurable` merged into its policy.
    async fn project(&self, name: &str, configurable: Value) -> Result<String> {
        let created = self
            .post(
                &self.human,
                "/v1/projects",
                json!({ "name": name, "members": [
                    {"hostId": self.host, "workspaceId": self.workspace, "role": "primary"}] }),
            )
            .await?;
        let project = created["id"].as_str().context("project id")?.to_string();
        let (status, body) = self
            .request(
                reqwest::Method::PATCH,
                &self.human,
                &format!("/v1/projects/{project}"),
                Some(json!({ "policy": { "configurable": configurable } })),
            )
            .await?;
        anyhow::ensure!(status == 200, "PATCH project {status}: {body}");
        Ok(project)
    }

    /// A human-seated project coordinator (a continuity lineage) and its
    /// agent token.
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
        let token = self
            .post(
                &self.human,
                &format!("/v1/instances/{id}/mcp-token"),
                json!({}),
            )
            .await?["token"]
            .as_str()
            .context("mcp token")?
            .to_string();
        Ok((id, token))
    }

    /// A child of `seat` on `host`, carried by `driver` for agent `kind`, in
    /// the seat's project scope (so the project's stall windows apply).
    async fn child(&self, seat: &str, host: &str, kind: &str, driver: &str) -> Result<String> {
        let record = self
            .hub
            .store()
            .context("store")?
            .insert_instance(
                host.to_owned(),
                None,
                kind.to_owned(),
                driver.to_owned(),
                None,
                json!({ "origin": "agent", "parentInstanceId": seat }),
            )
            .await?;
        let child = record.instance_id;
        self.db()?.execute(
            "UPDATE instances SET scope_json = (SELECT scope_json FROM instances WHERE id = ?1)
              WHERE id = ?2",
            rusqlite::params![seat, child],
        )?;
        Ok(child)
    }

    async fn journal(&self, host: &str, instance_id: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(host.to_owned(), instance_id.to_owned(), None, event)
            .await?;
        Ok(())
    }

    /// A live, busy child.
    async fn start(&self, host: &str, instance_id: &str) -> Result<()> {
        self.journal(host, instance_id, entity("running")).await?;
        self.journal(host, instance_id, activity("working")).await
    }

    async fn stalls(&self, token: &str, lineage: &str) -> Result<Vec<Value>> {
        let (status, body) = self
            .request(
                reqwest::Method::GET,
                token,
                &format!("/v1/lineages/{lineage}/inbox?limit=200"),
                None,
            )
            .await?;
        anyhow::ensure!(status == 200, "GET inbox {status}: {body}");
        Ok(body["items"]
            .as_array()
            .context("items")?
            .iter()
            .filter(|fact| fact["kind"] == "stalled")
            .cloned()
            .collect())
    }

    /// One tick and screen sweep `minutes` ahead; every host task awaited.
    async fn settle(&self, minutes: i64) -> Result<usize> {
        let (written, tasks) = self
            .hub
            .test_inbox_screen_sweep(&minutes_from_now(minutes), SWEEP_TIMEOUT)
            .await?;
        let mut total = written;
        for (_, task) in tasks {
            total += task.await?;
        }
        Ok(total)
    }

    /// [`Self::settle`] twice: a screen carrier's first read in a quiet
    /// period only records the screen; the second decides.
    async fn settle_twice(&self, minutes: i64) -> Result<usize> {
        Ok(self.settle(minutes).await? + self.settle(minutes).await?)
    }
}

fn minutes_from_now(minutes: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::minutes(minutes);
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("format")
}

fn entity(state: &str) -> Value {
    json!({ "kind": "lifecycle",
            "payload": { "type": "entity", "entityType": "instance", "state": state } })
}

fn activity(value: &str) -> Value {
    json!({ "kind": "lifecycle", "payload": {
        "type": "native", "topic": "session", "nativeName": "session",
        "status": { "state": "known", "value": value },
        "relatedIds": {}, "dataRef": null, "severity": "info", "affectsCompletion": false } })
}

fn tool_call(id: &str) -> Value {
    json!({ "kind": "tool_call", "payload": { "toolCallId": id,
        "toolName": { "state": "known", "value": "Bash" }, "state": "proposed" } })
}

fn tool_result(id: &str) -> Value {
    json!({ "kind": "tool_result", "payload": { "toolCallId": id, "stage": "final",
        "outcome": "succeeded" } })
}

/// A static screen: a tool header and a spinner whose glyph and elapsed
/// counter move but nothing else does.
const STILL_SCREEN: [&str; 3] = [
    "● Bash(sleep 400)",
    "  ⎿  Running…",
    "✻ Razzmatazzing… (6m 02s · ↓ 1.2k tokens · esc to interrupt)",
];

// ─── carrier fixtures: each carrier's own mapper ────────────────────────────

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// `{kind, payload}` of a serialized observation, the part the Hub folds.
fn folded(observation: Value) -> Value {
    json!({ "kind": observation["kind"], "payload": observation["payload"] })
}

fn tool_id(event: &Value) -> Option<&str> {
    event.pointer("/payload/toolCallId").and_then(Value::as_str)
}

/// One carrier's journaled tool events for its first call: every event that
/// opens or updates the call, then every event that closes it.
struct CarrierTool {
    label: &'static str,
    kind: &'static str,
    driver: &'static str,
    call: Vec<Value>,
    result: Vec<Value>,
}

/// Split a mapped event stream at the first call's first final result.
fn first_call(
    label: &'static str,
    kind: &'static str,
    driver: &'static str,
    events: Vec<Value>,
) -> CarrierTool {
    let tools: Vec<Value> = events
        .into_iter()
        .filter(|event| matches!(event["kind"].as_str(), Some("tool_call" | "tool_result")))
        .collect();
    let id = tools
        .iter()
        .find(|event| event["kind"] == "tool_call")
        .and_then(tool_id)
        .unwrap_or_else(|| panic!("{label}: the fixture maps no tool_call"))
        .to_owned();
    let mine: Vec<Value> = tools
        .into_iter()
        .filter(|event| tool_id(event) == Some(id.as_str()))
        .collect();
    let close = mine
        .iter()
        .position(|event| {
            event["kind"] == "tool_result"
                && event.pointer("/payload/stage").and_then(Value::as_str) == Some("final")
        })
        .unwrap_or_else(|| panic!("{label}: no final tool_result shares the call's toolCallId"));
    let (call, result) = mine.split_at(close);
    assert!(
        call.iter().all(|event| event["kind"] == "tool_call"),
        "{label}: the call opens before its result"
    );
    CarrierTool {
        label,
        kind,
        driver,
        call: call.to_vec(),
        result: result.to_vec(),
    }
}

/// claude-sdk / claude-print: the stream-json stdout mapper over a recorded
/// `--output-format stream-json` session.
fn claude_stream(driver: &'static str) -> CarrierTool {
    let kind = match driver {
        "claude-sdk" => remuda_protocol::DriverKind::ClaudeSdk,
        _ => remuda_protocol::DriverKind::ClaudePrint,
    };
    let source = std::fs::read_to_string(fixture(
        "remuda-testing/fixtures/claude/claude-permission-host-allow.jsonl",
    ))
    .expect("fixture");
    let mut mapper = remuda_driver::StdoutMapper::new(kind, "01993ab0-0000-7000-8600-8600000000ee");
    let mut events = Vec::new();
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).expect("fixture line");
        if value.get("_dir").and_then(Value::as_str) == Some("in") {
            continue;
        }
        for observation in mapper.map(value).expect("map frame") {
            events.push(folded(
                serde_json::to_value(&observation).expect("serialize"),
            ));
        }
    }
    first_call(driver, "claude", driver, events)
}

/// claude-pty and shell-pty claude: the transcript mapper over a recorded
/// interactive session's JSONL.
fn claude_transcript(driver: &'static str) -> CarrierTool {
    let source = std::fs::read_to_string(fixture(
        "remuda-testing/fixtures/claude-transcript/queue/interactive-session.jsonl",
    ))
    .expect("fixture");
    let mut mapper = remuda_driver::TranscriptMapper::new(
        remuda_protocol::DriverKind::ClaudePty,
        remuda_protocol::InstanceId::new(),
        remuda_protocol::RunId::new(),
        remuda_protocol::Id::new("obj").expect("id"),
        HostId::new(),
        "transcript-session".into(),
        "2.1.289".into(),
    );
    let mut events = Vec::new();
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        for observation in mapper.map_line(line).expect("map line") {
            events.push(folded(
                serde_json::to_value(&observation).expect("serialize"),
            ));
        }
    }
    first_call("claude transcript", "claude", driver, events)
}

/// shell-pty claude, live channel: the hook relay's PreToolUse / PostToolUse
/// folded by the signal layer, over a recorded hook session.
fn claude_hooks() -> CarrierTool {
    let source = std::fs::read_to_string(fixture(
        "remuda-testing/fixtures/hooks/claude-hook-session.jsonl",
    ))
    .expect("fixture");
    let mut live = remuda_signal::LiveState::new("hook-scope");
    let mut events = Vec::new();
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("fixture line");
        let event = remuda_signal::HookEvent::from_envelope(remuda_signal::HookEnvelope {
            credential: String::new(),
            event: record["event"].as_str().expect("event").to_owned(),
            ppid: record["ppid"].as_i64().unwrap_or(1) as i32,
            payload: record["payload"].clone(),
        });
        let mapped = remuda_signal::map_event(&event);
        let fold = live.observe(&event, &mapped, "2026-11-03T08:00:00.000Z", true);
        for payload in fold.extras {
            events.push(serde_json::to_value(&payload).expect("serialize"));
        }
    }
    first_call("claude hooks", "claude", "shell-pty", events)
}

fn adapter_home(dir: &Path) -> remuda_driver::adapters::AdapterHome {
    remuda_driver::adapters::AdapterHome {
        home: dir.to_path_buf(),
        cwd: dir.join("work"),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    }
}

fn adapter_events(observed: Vec<remuda_driver::adapters::AdapterObservation>) -> Vec<Value> {
    observed
        .iter()
        .map(|observed| serde_json::to_value(&observed.payload).expect("serialize"))
        .collect()
}

/// codex on shell-pty / generic-pty: the rollout file adapter over a recorded
/// interactive 0.154.0 rollout.
fn codex_rollout(driver: &'static str) -> CarrierTool {
    use remuda_driver::adapters::FileSignalAdapter as _;
    let dir = tempfile::tempdir().expect("dir");
    let rollout = dir.path().join("rollout.jsonl");
    std::fs::copy(
        fixture("remuda-driver/tests/fixtures/codex/interactive-0.154.0.jsonl"),
        &rollout,
    )
    .expect("copy fixture");
    let mut adapter = remuda_driver::adapters::CodexAdapter::new(adapter_home(dir.path()));
    adapter.bind_rollout("01a09c00-64d4-7ce1-8537-984e896b8e8a", rollout);
    let observed = adapter.poll().expect("poll");
    first_call("codex rollout", "codex", driver, adapter_events(observed))
}

/// grok on shell-pty / generic-pty: the session updates adapter over a
/// recorded TUI session.
fn grok_updates(driver: &'static str) -> CarrierTool {
    use remuda_driver::adapters::FileSignalAdapter as _;
    let dir = tempfile::tempdir().expect("dir");
    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("dir");
    std::fs::copy(
        fixture("remuda-driver/tests/fixtures/grok/tui-updates.jsonl"),
        session_dir.join("updates.jsonl"),
    )
    .expect("copy fixture");
    std::fs::write(session_dir.join("events.jsonl"), "").expect("events");
    std::fs::write(session_dir.join("usage.json"), "{}\n").expect("usage");
    let mut adapter = remuda_driver::adapters::GrokAdapter::new(adapter_home(dir.path()));
    adapter.bind_session_dir("01a09c24-46ef-7a03-9c89-88f1bc00bd0c", session_dir);
    let observed = adapter.poll().expect("poll");
    first_call("grok updates", "grok", driver, adapter_events(observed))
}

fn carriers() -> Vec<CarrierTool> {
    vec![
        claude_stream("claude-sdk"),
        claude_stream("claude-print"),
        claude_transcript("claude-pty"),
        claude_transcript("shell-pty"),
        claude_hooks(),
        codex_rollout("shell-pty"),
        codex_rollout("generic-pty"),
        grok_updates("shell-pty"),
        grok_updates("generic-pty"),
    ]
}

/// Per carrier: a long call yields no stall fact; once it ends and the child
/// stays quiet past the window, exactly one; with an explicit
/// `stallToolThresholdMins`, exactly one while the call is in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_carrier_journals_its_tool_in_flight_and_stalls_once() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    ctx.node().show(&STILL_SCREEN);
    let quiet = ctx
        .project("stall-quiet", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (quiet_seat, quiet_token) = ctx.coordinator(&quiet).await?;
    let tool = ctx
        .project(
            "stall-tool",
            json!({ "stallThresholdMins": 525_600, "stallToolThresholdMins": 15 }),
        )
        .await?;
    let (tool_seat, tool_token) = ctx.coordinator(&tool).await?;

    for carrier in carriers() {
        let label = carrier.label;
        let driver = carrier.driver;
        // The quiet rule around a long call.
        let child = ctx
            .child(&quiet_seat, &ctx.host, carrier.kind, driver)
            .await?;
        ctx.start(&ctx.host, &child).await?;
        for event in &carrier.call {
            ctx.journal(&ctx.host, &child, event.clone()).await?;
        }
        let reads = ctx.node().reads();
        for minutes in [30, 120, 600] {
            assert_eq!(
                ctx.settle_twice(minutes).await?,
                0,
                "{label}/{driver}: in flight at +{minutes}m"
            );
        }
        assert_eq!(
            ctx.node().reads(),
            reads,
            "{label}/{driver}: a call in flight never reaches the screen fallback"
        );
        for event in &carrier.result {
            ctx.journal(&ctx.host, &child, event.clone()).await?;
        }
        assert_eq!(
            ctx.settle_twice(19).await?,
            0,
            "{label}/{driver}: inside the window"
        );
        let mut recorded = 0;
        for minutes in [21, 21, 60, 240] {
            recorded += ctx.settle_twice(minutes).await?;
        }
        assert_eq!(recorded, 1, "{label}/{driver}: one quiet period, one fact");
        let facts: Vec<Value> = ctx
            .stalls(&quiet_token, &quiet_seat)
            .await?
            .into_iter()
            .filter(|fact| fact["subjectInstanceId"] == child.as_str())
            .collect();
        assert_eq!(facts.len(), 1, "{label}/{driver}: {facts:#?}");
        assert_eq!(facts[0]["payload"]["cause"], "quiet", "{label}/{driver}");
        assert_eq!(facts[0]["payload"]["thresholdMins"], 20, "{label}/{driver}");
        // Finish the child so it is not busy for the next carrier's ticks.
        ctx.journal(&ctx.host, &child, entity("exited")).await?;

        // An explicit tool window: one fact while the call is in flight.
        let hung = ctx
            .child(&tool_seat, &ctx.host, carrier.kind, driver)
            .await?;
        ctx.start(&ctx.host, &hung).await?;
        for event in &carrier.call {
            ctx.journal(&ctx.host, &hung, event.clone()).await?;
        }
        assert_eq!(
            ctx.settle_twice(14).await?,
            0,
            "{label}/{driver}: inside the tool window"
        );
        let mut recorded = 0;
        for minutes in [16, 30, 600] {
            recorded += ctx.settle_twice(minutes).await?;
        }
        assert_eq!(recorded, 1, "{label}/{driver}: one call, one tool stall");
        let facts: Vec<Value> = ctx
            .stalls(&tool_token, &tool_seat)
            .await?
            .into_iter()
            .filter(|fact| fact["subjectInstanceId"] == hung.as_str())
            .collect();
        assert_eq!(facts.len(), 1, "{label}/{driver}: {facts:#?}");
        assert_eq!(
            facts[0]["payload"]["cause"], "tool-in-flight",
            "{label}/{driver}"
        );
        assert_eq!(
            facts[0]["payload"]["toolCallId"].as_str(),
            tool_id(&carrier.call[0]),
            "{label}/{driver}"
        );
        ctx.journal(&ctx.host, &hung, entity("exited")).await?;
    }
    Ok(())
}

/// Parallel calls: the child is in flight until the LAST result, and a late
/// copy of a finished call (a second channel replaying it) does not reopen
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_calls_are_in_flight_until_the_last_result() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-parallel", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let child = ctx.child(&seat, &ctx.host, "claude", "claude-sdk").await?;
    ctx.start(&ctx.host, &child).await?;
    for id in ["tc_a", "tc_b", "tc_c"] {
        ctx.journal(&ctx.host, &child, tool_call(id)).await?;
    }
    ctx.journal(&ctx.host, &child, tool_result("tc_b")).await?;
    ctx.journal(&ctx.host, &child, tool_result("tc_a")).await?;
    // The hook channel's copy of a finished call lands after its result.
    ctx.journal(&ctx.host, &child, tool_call("tc_a")).await?;
    assert_eq!(ctx.settle(120).await?, 0, "tc_c is still running");
    assert!(ctx.stalls(&token, &seat).await?.is_empty());

    ctx.journal(&ctx.host, &child, tool_result("tc_c")).await?;
    assert_eq!(ctx.settle(19).await?, 0);
    assert_eq!(
        ctx.settle(21).await?,
        1,
        "the last result restarts the window"
    );
    assert_eq!(ctx.settle(240).await?, 0);
    assert_eq!(ctx.stalls(&token, &seat).await?.len(), 1);
    Ok(())
}

/// A Hub restart mid-call: the tracker is durable state folded from the
/// journal, so the call is still in flight after the restart (no spurious
/// stall), and the window counts from its result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_restart_mid_call_rebuilds_without_a_spurious_stall() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-restart", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let child = ctx.child(&seat, &ctx.host, "claude", "claude-sdk").await?;
    ctx.start(&ctx.host, &child).await?;
    ctx.journal(&ctx.host, &child, tool_call("tc_across_restart"))
        .await?;
    let host = ctx.host.clone();

    // The restart drops the Node link; the host-offline fact that follows is
    // not a stall, so stall facts are read from the inbox.
    let ctx = ctx.restart().await?;
    for minutes in [21, 120, 600] {
        ctx.settle(minutes).await?;
        assert!(
            ctx.stalls(&token, &seat).await?.is_empty(),
            "+{minutes}m after the restart"
        );
    }
    ctx.journal(&host, &child, tool_result("tc_across_restart"))
        .await?;
    ctx.settle(19).await?;
    assert!(ctx.stalls(&token, &seat).await?.is_empty());
    ctx.settle(21).await?;
    ctx.settle(60).await?;
    let facts = ctx.stalls(&token, &seat).await?;
    assert_eq!(facts.len(), 1, "{facts:#?}");
    assert_eq!(facts[0]["payload"]["cause"], "quiet");
    Ok(())
}

/// The screen fallback reads a screen only once the journal has been quiet
/// for the window. The first read is the baseline the screen's quiet counts
/// from; a screen that changes restarts the quiet period; a still one yields
/// exactly one fact per quiet period.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_screen_fallback_runs_only_after_journal_quiet() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-screen", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    // A carrier with no tool channel at all: agy on shell-pty.
    let child = ctx.child(&seat, &ctx.host, "agy", "shell-pty").await?;
    ctx.start(&ctx.host, &child).await?;
    ctx.node().show(&["$ make", "building 1/40"]);

    assert_eq!(ctx.settle_twice(19).await?, 0);
    assert_eq!(ctx.node().reads(), 0, "the journal is not quiet yet");

    // The first read is the baseline: nothing for a full window after it.
    assert_eq!(ctx.settle(21).await?, 0, "the first read only records");
    assert_eq!(ctx.node().reads(), 1);
    assert_eq!(
        ctx.settle_twice(40).await?,
        0,
        "inside the baseline's window"
    );
    assert_eq!(ctx.node().reads(), 1, "no candidate inside the window");

    // Past it the screen has moved: progress, the quiet period restarts.
    ctx.node()
        .show(&["$ make", "building 1/40", "building 2/40"]);
    assert_eq!(ctx.settle(42).await?, 0, "a changed screen is progress");
    assert_eq!(ctx.node().reads(), 2);
    assert_eq!(
        ctx.settle_twice(61).await?,
        0,
        "inside the restarted window"
    );
    assert_eq!(ctx.settle(63).await?, 1, "still past the window: one fact");
    assert_eq!(ctx.node().reads(), 3);
    for minutes in [80, 240] {
        assert_eq!(ctx.settle_twice(minutes).await?, 0, "same quiet period");
    }
    let facts = ctx.stalls(&token, &seat).await?;
    assert_eq!(facts.len(), 1, "{facts:#?}");
    assert_eq!(facts[0]["payload"]["cause"], "quiet");
    assert_eq!(facts[0]["payload"]["source"], "screen");
    Ok(())
}

/// r2 item 3: a screen that repainted moments before the first read of a
/// quiet period is not stamped stalled at once. The first read is the
/// baseline (as `observe_one` takes its first observation), so the fact
/// waits a full window after it, and the quiet period starts at it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_screen_read_is_the_baseline() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-baseline", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let child = ctx.child(&seat, &ctx.host, "claude", "shell-pty").await?;
    ctx.start(&ctx.host, &child).await?;
    ctx.node().show(&STILL_SCREEN);

    // The journal has been quiet for the window, but the Hub has never seen
    // this screen: two reads in a row decide nothing.
    assert_eq!(ctx.settle_twice(30).await?, 0);
    assert_eq!(
        ctx.node().reads(),
        1,
        "the second tick is inside the window"
    );
    assert!(ctx.stalls(&token, &seat).await?.is_empty());
    assert_eq!(ctx.settle(49).await?, 0);
    assert_eq!(ctx.settle(51).await?, 1, "a full window after the baseline");
    let facts = ctx.stalls(&token, &seat).await?;
    assert_eq!(facts.len(), 1, "{facts:#?}");
    let since = facts[0]["payload"]["since"].as_str().context("since")?;
    let since = time::OffsetDateTime::parse(since, &time::format_description::well_known::Rfc3339)?;
    let baseline = time::OffsetDateTime::parse(
        &minutes_from_now(30),
        &time::format_description::well_known::Rfc3339,
    )?;
    assert!(
        (since - baseline).abs() < time::Duration::minutes(1),
        "the quiet period starts at the first read, not at the journal's last row: {since}"
    );
    Ok(())
}

/// r2 item 1: claude-pty and generic-pty have no `screen_read`, so they are
/// not screen carriers: a quiet child keeps the journal quiet rule (exactly
/// one fact) and costs no `tty.screen` RPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_pty_and_generic_pty_keep_the_journal_quiet_rule() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-pty", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let claude = ctx.child(&seat, &ctx.host, "claude", "claude-pty").await?;
    let codex = ctx.child(&seat, &ctx.host, "codex", "generic-pty").await?;
    for child in [&claude, &codex] {
        ctx.start(&ctx.host, child).await?;
    }
    ctx.node().show(&STILL_SCREEN);
    assert_eq!(ctx.settle(19).await?, 0);
    assert_eq!(ctx.settle(21).await?, 2, "one quiet stall per child");
    for minutes in [60, 240] {
        assert_eq!(ctx.settle(minutes).await?, 0, "same quiet period");
    }
    assert_eq!(ctx.node().reads(), 0, "no tty.screen RPC for them");
    let facts = ctx.stalls(&token, &seat).await?;
    for child in [&claude, &codex] {
        let mine: Vec<&Value> = facts
            .iter()
            .filter(|fact| fact["subjectInstanceId"] == child.as_str())
            .collect();
        assert_eq!(mine.len(), 1, "{facts:#?}");
        assert_eq!(mine[0]["payload"]["cause"], "quiet");
        assert!(mine[0]["payload"].get("source").is_none());
    }
    Ok(())
}

/// r2 item 1, defence: a screen carrier whose Node answers `supported:
/// false` (an adopted PTY, a dropped one) is not a screen carrier for that
/// evaluation: the journal quiet rule records its one fact. An unreachable
/// host still records nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_screen_falls_back_to_the_journal_quiet_rule() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-unsupported", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let child = ctx.child(&seat, &ctx.host, "claude", "shell-pty").await?;
    ctx.start(&ctx.host, &child).await?;

    // A host that does not answer in time: no fact.
    ctx.node().mute();
    let (_, tasks) = ctx
        .hub
        .test_inbox_screen_sweep(&minutes_from_now(21), Duration::from_millis(1))
        .await?;
    for (_, task) in tasks {
        assert_eq!(task.await?, 0, "an unreachable screen records nothing");
    }
    assert!(ctx.stalls(&token, &seat).await?.is_empty());

    ctx.node().unsupported();
    assert_eq!(ctx.settle(22).await?, 1, "the journal quiet fact");
    for minutes in [60, 240] {
        assert_eq!(ctx.settle(minutes).await?, 0, "same quiet period");
    }
    let facts = ctx.stalls(&token, &seat).await?;
    assert_eq!(facts.len(), 1, "{facts:#?}");
    assert_eq!(facts[0]["payload"]["cause"], "quiet");
    assert!(facts[0]["payload"].get("source").is_none());
    Ok(())
}

/// r2 item 2: with `stallToolThresholdMins` set, every call in flight past
/// the window gets its own fact, not only the oldest; later ticks add none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_hung_parallel_call_gets_its_own_tool_stall() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project(
            "stall-tool-parallel",
            json!({ "stallThresholdMins": 525_600, "stallToolThresholdMins": 15 }),
        )
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let child = ctx.child(&seat, &ctx.host, "claude", "claude-sdk").await?;
    ctx.start(&ctx.host, &child).await?;
    for id in ["tc_older", "tc_younger"] {
        ctx.journal(&ctx.host, &child, tool_call(id)).await?;
    }
    assert_eq!(ctx.settle(14).await?, 0);
    assert_eq!(ctx.settle(16).await?, 2, "one fact per hung call");
    for minutes in [30, 60, 600] {
        assert_eq!(ctx.settle(minutes).await?, 0, "+{minutes}m adds nothing");
    }
    let facts = ctx.stalls(&token, &seat).await?;
    let mut calls: Vec<&str> = facts
        .iter()
        .filter_map(|fact| fact["payload"]["toolCallId"].as_str())
        .collect();
    calls.sort_unstable();
    assert_eq!(calls, ["tc_older", "tc_younger"], "{facts:#?}");
    Ok(())
}

/// A screen that shows a DONE report or a dialog is not a stall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_screen_classification_decides_the_fact() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-classify", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let child = ctx.child(&seat, &ctx.host, "codex", "shell-pty").await?;
    ctx.start(&ctx.host, &child).await?;
    ctx.node().show(&[
        "all tests passed",
        "DONE 0123456789abcdef0123456789abcdef01234567",
    ]);
    for minutes in [21, 60, 240] {
        assert_eq!(ctx.settle_twice(minutes).await?, 0, "+{minutes}m");
    }
    assert!(ctx.stalls(&token, &seat).await?.is_empty());
    assert!(
        ctx.node().reads() >= 3,
        "the screen was read and classified"
    );
    Ok(())
}

/// Per host, in its own task: a host whose Node never answers holds only its
/// own sweep. The other host's fact is recorded while the dead host's read is
/// still waiting, and the next tick skips only the host still being swept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_dead_host_does_not_delay_another_hosts_sweep() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-hosts", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let (live_host, live_node) = ctx.second_host().await?;
    ctx.node().mute();
    live_node.show(&STILL_SCREEN);
    let dead_child = ctx.child(&seat, &ctx.host, "claude", "shell-pty").await?;
    let live_child = ctx.child(&seat, &live_host, "claude", "shell-pty").await?;
    ctx.start(&ctx.host, &dead_child).await?;
    ctx.start(&live_host, &live_child).await?;

    // The first sweep records the live screen (the baseline); the dead
    // host's read hangs.
    let (_, tasks) = ctx
        .hub
        .test_inbox_screen_sweep(&minutes_from_now(21), SWEEP_TIMEOUT)
        .await?;
    let mut dead = None;
    for (host, task) in tasks {
        if host == ctx.host {
            dead = Some(task);
        } else {
            assert_eq!(task.await?, 0, "the first read only records");
        }
    }
    let dead = dead.context("a task for the dead host")?;
    // A window after the baseline the live host decides; the dead one is
    // still being swept, so it gets no second task.
    let (_, tasks) = ctx
        .hub
        .test_inbox_screen_sweep(&minutes_from_now(42), SWEEP_TIMEOUT)
        .await?;
    assert_eq!(
        tasks
            .iter()
            .map(|(host, _)| host.as_str())
            .collect::<Vec<_>>(),
        vec![live_host.as_str()],
        "a host still being swept is skipped, the other is not"
    );
    for (_, task) in tasks {
        assert_eq!(task.await?, 1, "the live host's stall fact");
    }
    assert!(
        !dead.is_finished(),
        "the dead host's read is still waiting while the live host's fact landed"
    );
    let facts = ctx.stalls(&token, &seat).await?;
    assert_eq!(facts.len(), 1, "{facts:#?}");
    assert_eq!(facts[0]["subjectInstanceId"], live_child.as_str());
    assert!(ctx.node().reads() >= 1);
    dead.abort();

    // An unreadable screen times out into no fact, never an error.
    let (_, tasks) = ctx
        .hub
        .test_inbox_screen_sweep(&minutes_from_now(43), Duration::from_millis(1))
        .await?;
    for (host, task) in tasks {
        if host == ctx.host {
            assert_eq!(task.await?, 0, "a timed-out read records nothing");
        } else {
            task.await?;
        }
    }
    assert_eq!(ctx.stalls(&token, &seat).await?.len(), 1);
    Ok(())
}

// ─── logs ───────────────────────────────────────────────────────────────────

fn sweep_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "remuda_hub::inbox=trace,remuda_hub::stall_screen=trace",
            ))
            .with_ansi(false)
            .with_writer(move || Sink(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one log capture");
        buffer
    })
    .clone()
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The sweep logs categories only: the fact's kind and outcome, the
/// classification and the unreadable category; never a screen row or an id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_logs_carry_categories_never_screen_text() -> Result<()> {
    let logs = sweep_logs();
    let ctx = Ctx::spawn().await?;
    let project = ctx
        .project("stall-logs", json!({ "stallThresholdMins": 20 }))
        .await?;
    let (seat, token) = ctx.coordinator(&project).await?;
    let marker = "screen-row-QX93-confidential";
    let still = ctx.child(&seat, &ctx.host, "agy", "shell-pty").await?;
    let done = ctx.child(&seat, &ctx.host, "agy", "shell-pty").await?;
    ctx.start(&ctx.host, &still).await?;
    ctx.start(&ctx.host, &done).await?;
    ctx.node().show(&[
        "$ deploy",
        marker,
        "DONE 0123456789abcdef0123456789abcdef01234567",
    ]);
    // Both screens read alike, so both classify `done`: no fact yet.
    assert_eq!(ctx.settle(21).await?, 0, "the baseline");
    assert_eq!(ctx.settle(41).await?, 0, "classified done");
    ctx.journal(&ctx.host, &done, entity("exited")).await?;
    ctx.node().show(&["$ deploy", marker]);
    assert_eq!(ctx.settle(42).await?, 0, "a changed screen restarts");
    assert_eq!(ctx.settle(63).await?, 1);
    assert_eq!(ctx.stalls(&token, &seat).await?.len(), 1);
    // An unreadable screen: the category only.
    ctx.node().mute();
    let (_, tasks) = ctx
        .hub
        .test_inbox_screen_sweep(&minutes_from_now(90), Duration::from_millis(1))
        .await?;
    for (_, task) in tasks {
        task.await?;
    }

    let text = String::from_utf8(logs.lock().expect("log buffer").clone())?;
    assert!(
        text.lines().any(|line| line.contains("inbox fact")
            && line.contains("kind=\"stalled\"")
            && line.contains("outcome=\"recorded\"")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.contains("not a stall") && line.contains("class=\"done\"")),
        "{text}"
    );
    assert!(
        text.lines().any(|line| line.contains("unreadable")
            && line.contains("code=\"screen-unavailable\"")),
        "{text}"
    );
    assert!(!text.contains(marker), "{text}");
    assert!(!text.contains("0123456789abcdef"), "{text}");
    assert!(
        !text.contains(&still) && !text.contains(&done),
        "no ids: {text}"
    );
    Ok(())
}
