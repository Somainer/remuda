//! M1 batch 5b (co-watch): roster state transitions driven by `remuda watch`
//! observations against a fake Node that serves scripted `tty.screen` frames,
//! plus the worker intervention verbs (nudge / answer / switch-model / resume /
//! replace / stop).
//!
//! Every classification is exercised: new DONE vs an echoed known tip,
//! BLOCKED, idle-after-API-error, stalled, and gone; the worker verbs are
//! asserted through the exact Hub → Node RPCs they drive.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Scripted screen the fake Node serves to `tty.screen`.
#[derive(Clone)]
struct ScreenState {
    supported: bool,
    lifecycle: String,
    /// `emulator` (row grid) or `raw-ring` (ANSI-stripped tail).
    source: String,
    lines: Vec<String>,
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
            supported: true,
            lifecycle: "ready".to_string(),
            source: "emulator".to_string(),
            lines: Vec::new(),
        }
    }
}

/// A connected fake Node serving worker RPCs, a scripted screen, and a fresh
/// instance id on every create/resume.
struct FakeNode {
    _task: tokio::task::JoinHandle<()>,
    screen: Arc<Mutex<ScreenState>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// Queued Node→Hub JSON-RPC frames (scripted `journal.append` events),
    /// drained by the node task right before its next RPC reply.
    feed: mpsc::UnboundedSender<Value>,
    feed_seq: Arc<AtomicU64>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self._task.abort();
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

async fn enroll_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspaces: &[&str],
) -> Result<FakeNode> {
    let screen = Arc::new(Mutex::new(ScreenState::default()));
    let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::new(AtomicU64::new(1));
    let (feed_tx, mut feed_rx) = mpsc::unbounded_channel::<Value>();
    let feed_seq = Arc::new(AtomicU64::new(1));

    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    let ws_rows: Vec<Value> = workspaces
        .iter()
        .map(|id| json!({ "workspaceId": id, "hostId": host_id, "root": format!("/tmp/{id}") }))
        .collect();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": format!("fake-{host_id}"),
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "labels": {"toolchain": "rust"},
                    "maxInstances": 8,
                    "resources": {"cpuCount": 8, "cpuPct": 5, "memPct": 20,
                                  "loadAvg1": 0.4, "diskFreeGb": 120.0},
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr": {"version": "0.9.0", "socket": "/tmp/fake-herdr.sock"},
                    "workspaces": ws_rows,
                    "workspaceRevision": workspaces.len() as u64,
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["nodeToken"].is_string(), "hello {hello}");

    let (screen_task, calls_task, counter_task) = (screen.clone(), calls.clone(), counter.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            // Flush scripted node→hub frames before answering this RPC: while
            // the hub awaits this reply it issues no other RPC on the
            // connection, so every inbound frame here is an append ack, and
            // the journal is committed before the hub reads it post-reply.
            while let Ok(feed) = feed_rx.try_recv() {
                if node
                    .send(Message::Text(feed.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                if recv_json(&mut node).await.is_err() {
                    break;
                }
            }
            if let Some(id) = frame.get("id")
                && let Some(method) = frame.get("method").and_then(Value::as_str)
            {
                calls_task.lock().unwrap().push((
                    method.to_string(),
                    frame.get("params").cloned().unwrap_or(json!({})),
                ));
                let seq = counter_task.fetch_add(1, Ordering::SeqCst);
                let instance_id = format!("ins_fake{seq:024}");
                let result = match method {
                    "worker.provision" => {
                        let params = frame.get("params").cloned().unwrap_or(json!({}));
                        let name = params["name"].as_str().unwrap_or("x");
                        let branch = params["branch"].as_str().unwrap_or("wt/x/work");
                        json!({
                            "name": name, "branch": branch, "startPoint": "origin/main",
                            "worktreePath": format!("/tmp/remuda-wt/{name}"),
                            "targetDir": format!("/tmp/remuda-target/{name}"),
                        })
                    }
                    "worker.remove" => json!({
                        "name": frame["params"]["name"].as_str().unwrap_or("x"),
                        "worktreeRemoved": true, "targetRemoved": true,
                        "reclaimedBytes": "4096",
                    }),
                    "instance.create" | "instance.resume" => {
                        json!({ "accepted": true, "instanceId": instance_id })
                    }
                    "instance.send" | "instance.close" | "instance.cancel" => {
                        json!({ "accepted": true })
                    }
                    "tty.screen" => {
                        let state = screen_task.lock().unwrap().clone();
                        json!({
                            "instanceId": frame["params"]["instanceId"],
                            "supported": state.supported,
                            "lifecycle": state.lifecycle,
                            "driver": "claude-pty",
                            "cols": 80, "rows": 24,
                            "source": state.source,
                            "lines": state.lines,
                        })
                    }
                    _ => json!({ "ok": true }),
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
        }
    });
    Ok(FakeNode {
        _task: task,
        screen,
        calls,
        feed: feed_tx,
        feed_seq,
    })
}

impl FakeNode {
    fn set_screen(&self, lines: &[&str], lifecycle: &str) {
        *self.screen.lock().unwrap() = ScreenState {
            supported: true,
            lifecycle: lifecycle.to_string(),
            source: "emulator".to_string(),
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
        };
    }

    /// Serve a headless raw VT ring tail instead of an emulated row grid.
    fn set_screen_raw(&self, lines: &[&str], lifecycle: &str) {
        *self.screen.lock().unwrap() = ScreenState {
            supported: true,
            lifecycle: lifecycle.to_string(),
            source: "raw-ring".to_string(),
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
        };
    }

    /// A print/dead driver: `tty.screen` answers unsupported, so the Hub must
    /// classify from the instance record and journal.
    fn set_print(&self, lifecycle: &str) {
        *self.screen.lock().unwrap() = ScreenState {
            supported: false,
            lifecycle: lifecycle.to_string(),
            source: "emulator".to_string(),
            lines: Vec::new(),
        };
    }

    fn set_gone(&self) {
        *self.screen.lock().unwrap() = ScreenState {
            supported: false,
            lifecycle: "closed".to_string(),
            source: "emulator".to_string(),
            lines: Vec::new(),
        };
    }

    /// Queue `journal.append` events for an instance; they flush on the next
    /// hub→node RPC.
    fn append_journal(&self, instance_id: &str, events: &[Value]) {
        for event in events {
            let seq = self.feed_seq.fetch_add(1, Ordering::SeqCst);
            self.feed
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": format!("feed-{seq}"),
                    "method": "journal.append",
                    "params": { "instanceId": instance_id, "event": event },
                }))
                .unwrap();
        }
    }

    fn payloads(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

const BRIEF: &str =
    "Do the tiny task in $WORKTREE.\nReply on one line: DONE <sha> or BLOCKED <reason>.\n";

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    node: FakeNode,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("watch-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = enroll_node(&hub, &host, &[&workspace]).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            node,
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        let mut builder = self
            .http
            .request(method.parse().unwrap(), format!("{}{}", self.base(), path))
            .bearer_auth(&self.human);
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(json!(null));
        (status, body)
    }

    /// Dispatch via the API and return (worker id, name).
    async fn dispatch(&self, project_id: &str, name: &str, range: &str) -> (String, String) {
        let _ = range;
        let body = json!({
            "projectId": project_id, "brief": BRIEF, "briefName": "brief.md",
            "harness": "claude", "name": name,
        });
        let (status, body) = self
            .request("POST", "/v1/workers/dispatch", Some(body))
            .await;
        assert!(status.is_success(), "dispatch {status}: {body}");
        (
            body["worker"]["id"].as_str().unwrap().to_string(),
            body["worker"]["name"].as_str().unwrap().to_string(),
        )
    }

    async fn observe(&self) -> Value {
        let (status, body) = self
            .request("POST", "/v1/workers/observe", Some(json!({})))
            .await;
        assert_eq!(status, 200, "observe {body}");
        body
    }
}

/// The fake node enrolled exactly one workspace, but project creation needs
/// that workspace as a member. Create the project through the API then replace
/// its member list with the enrolled host's only workspace id, which the Hub
/// already recorded on hello.
async fn project_with_enrolled_workspace(ctx: &Ctx, name: &str, range: &str) -> String {
    // Discover the enrolled workspace id from the host record.
    let (_, hosts) = ctx.request("GET", "/v1/hosts", None).await;
    let workspace_id = hosts["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|h| h["hostId"].as_str() == Some(ctx.host.as_str()))
        })
        .and_then(|host| host["workspaces"].as_array())
        .and_then(|workspaces| workspaces.first())
        .and_then(|workspace| workspace.get("workspaceId"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let body = json!({
        "name": name,
        "members": [{ "hostId": ctx.host, "workspaceId": workspace_id, "role": "build" }],
        "hosts": [{
            "hostId": ctx.host, "maxInstances": 8, "maxBuilding": 4,
            "diskBudgetGb": 10, "portBlocks": [range],
            "requires": ["toolchain=rust"], "latencyClass": "remote",
        }],
    });
    let (status, created) = ctx.request("POST", "/v1/projects", Some(body)).await;
    assert!(status.is_success(), "{created}");
    created["id"].as_str().unwrap().to_string()
}

// ── classification ─────────────────────────────────────────────────────────

#[tokio::test]
async fn new_done_then_echo_transitions_roster() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-done", "58600-58629").await;
    let (id, name) = ctx.dispatch(&project, "c-done", "58600-58629").await;

    // A fresh DONE sha on screen → done, lifecycle state moves too.
    ctx.node
        .set_screen(&["working…", "DONE 0a10ebf351aa"], "ready");
    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(row["watch"]["status"], "done");
    assert_eq!(row["watch"]["sha"], "0a10ebf351aa");
    assert_eq!(row["state"]["state"], "done");
    assert_eq!(row["state"]["sha"], "0a10ebf351aa");

    // The same DONE re-shown (a resumed/rescrolled session) is an echo: the
    // watch status is no longer "done", but the durable claim stays done.
    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(row["watch"]["status"], "working", "{row}");
    assert_eq!(row["state"]["state"], "done");
    let _ = id;
    let _ = name;
}

#[tokio::test]
async fn blocked_dialog_is_classified_and_echo_suppressed() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-blocked", "58630-58659").await;
    ctx.dispatch(&project, "c-block", "58630-58659").await;

    ctx.node
        .set_screen(&["• BLOCKED need permission to read secrets"], "ready");
    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(row["watch"]["status"], "blocked");
    assert_eq!(row["watch"]["reason"], "need permission to read secrets");
    assert_eq!(row["state"]["state"], "blocked");

    // Same reason re-shown is an echo.
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "working");
    // But the brief contract placeholder never classifies as blocked.
    ctx.node.set_screen(
        &["When done reply on one line: DONE <sha> or BLOCKED <reason>"],
        "ready",
    );
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "working");
}

#[tokio::test]
async fn first_run_dialog_screens_classify_blocked_with_their_titles() {
    // dispatch-onboarding-1: watch must never print "working" for a parked
    // first-run dialog. Both dialog layouts, emulated grid and raw ring tail.
    let cases: &[(&str, &[&str], &str)] = &[
        (
            "58630-58659",
            &[
                "Welcome to Claude Code!",
                "",
                "Quick safety check:",
                "Is this a project you created or one you trust?",
                "",
                "❯ No, exit",
                "  Yes, I trust this folder",
            ],
            "Is this a project you created or one you trust?",
        ),
        (
            "58660-58689",
            &[
                "Read outside the working directories",
                "Allow reads outside the working directories?",
                "❯ Yes, keep allowing reads outside the working directories",
                "  No, block reads outside the working directories from now on",
                "  No, ask again next time",
            ],
            "Allow reads outside the working directories?",
        ),
    ];
    for (range, screen, title) in cases {
        let ctx = Ctx::spawn().await.unwrap();
        let project = project_with_enrolled_workspace(&ctx, "watch-dialog", range).await;
        ctx.dispatch(&project, "c-dialog", range).await;
        // The carrier guesses "ready/idle"; the dialog text must win anyway.
        ctx.node.set_screen(screen, "ready");
        let observed = ctx.observe().await;
        let row = &observed["items"][0];
        assert_eq!(row["watch"]["status"], "blocked", "{title}: {row}");
        assert_eq!(row["watch"]["reason"], *title, "{title}");
        assert_eq!(row["state"]["state"], "blocked");
        // Unlike a worker BLOCKED report, a modal still on screen is not an
        // echo: the second observation keeps reporting blocked.
        let observed = ctx.observe().await;
        assert_eq!(
            observed["items"][0]["watch"]["status"], "blocked",
            "{title} must not collapse to working while still on screen"
        );
    }
}

#[tokio::test]
async fn first_run_dialog_classifies_from_a_raw_ring_tail() {
    // The headless carrier (no live viewer) returns a raw VT ring tail; the
    // dialog rule must fire there too, for both carriers' screens.
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-dialog-raw", "58690-58719").await;
    ctx.dispatch(&project, "c-dialog-raw", "58690-58719").await;
    ctx.node.set_screen_raw(
        &[
            "❯ probe                                                              ",
            "Quick safety check:                                                  ",
            "Is this a project you created or one you trust?                      ",
            "❯ No, exit                                                           ",
            "  Yes, I trust this folder                                           ",
        ],
        "ready",
    );
    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(row["watch"]["status"], "blocked", "{row}");
    assert_eq!(
        row["watch"]["reason"],
        "Is this a project you created or one you trust?"
    );
}

#[tokio::test]
async fn idle_after_api_error_is_classified() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-api", "58660-58689").await;
    ctx.dispatch(&project, "c-api", "58660-58689").await;
    ctx.node
        .set_screen(&["API Error: upstream returned 502", "Retrying…"], "ready");
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "idle-api-error");
    // An API blip while busy is NOT idle-after-error.
    ctx.node.set_screen(
        &["API Error: upstream returned 502", "Retrying…"],
        "running",
    );
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "working");
}

#[tokio::test]
async fn stalled_requires_an_aged_quiet_turn() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-stall", "58690-58719").await;
    let (id, _name) = ctx.dispatch(&project, "c-stall", "58690-58719").await;
    ctx.node.set_screen(&["thinking…"], "running");

    // First observation seeds activity at "now": not stalled yet.
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "working");

    // Age the recorded last-activity by 31 minutes, keep the screen identical.
    let aged = time::OffsetDateTime::now_utc() - time::Duration::minutes(31);
    let millis = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
    )
    .unwrap();
    let aged = aged.format(&millis).unwrap();
    ctx.hub
        .age_worker_last_activity_for_tests(&id, &aged)
        .await
        .unwrap();

    let observed = ctx.observe().await;
    assert_eq!(
        observed["items"][0]["watch"]["status"], "stalled",
        "{observed}"
    );
}

/// A row the Hub settled with `node-epoch-changed` must classify gone *with
/// that reason*.
///
/// The 2026-09-18 demo: after the Node restarted, `remuda watch` kept calling
/// the dead sessions working. Even once the row settles, reporting a bare
/// carrier code (`host-offline`, `instance-closed`) throws away the one fact
/// the owner can act on — the process is gone because the Node restarted, and
/// the conversation can be resumed. The classification must carry it.
#[tokio::test]
async fn gone_carries_the_settled_rows_own_reason() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-epoch", "58930-58959").await;
    let (worker_id, _) = ctx.dispatch(&project, "c-epoch", "58930-58959").await;

    // Settle the roster row's instance exactly as the Hub's epoch reconcile
    // does, then take the Node away so the screen is unreadable too: both
    // paths must agree on the reason rather than the weaker carrier code.
    let instance_id = {
        let observed = ctx.observe().await;
        observed["items"][0]["instanceId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    ctx.hub
        .store()
        .expect("store")
        .settle_instance_exited(instance_id.clone(), "node-epoch-changed".into())
        .await
        .expect("settle");
    ctx.hub.test_disconnect_node(&ctx.host).await;

    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(
        row["watch"]["status"], "gone",
        "a settled session with no carrier must read gone: {row}"
    );
    assert_eq!(
        row["watch"]["reason"], "node-epoch-changed",
        "the settled row's own reason must survive into the classification: {row}"
    );
    // `detail` prefixes where the read came from, so a reader can tell a
    // journal classification from a screen one; the reason travels whole in
    // its own field, which is what the CLI's reason column prints.
    let detail = row["watch"]["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("node-epoch-changed"),
        "detail must repeat the reason: {detail}"
    );
    let (_, roster) = ctx.request("GET", "/v1/workers", None).await;
    let persisted = roster["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(worker_id))
        .expect("roster row");
    assert_eq!(persisted["watch"]["reason"], "node-epoch-changed");
}

#[tokio::test]
async fn gone_when_carrier_reports_closed() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-gone", "58720-58749").await;
    ctx.dispatch(&project, "c-gone", "58720-58749").await;
    ctx.node.set_gone();
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "gone");
}

// ── failed first turn (watch-failed-1) ─────────────────────────────────────

/// An assistant message observation carrying the harness's API error text.
fn assistant_error_event(message: &str) -> Value {
    json!({
        "kind": "message",
        "payload": {
            "role": "assistant",
            "phase": "final",
            "blocks": [{ "type": "text", "text": message }],
        },
    })
}

/// The print driver's turn-result frame: `result` with status `error` (this is
/// the event that also drives the Hub instance lifecycle to `failed`).
fn turn_error_event() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "turn",
            "nativeName": "result",
            "status": { "value": "error" },
            "affectsCompletion": true,
            "relatedIds": { "resultIndex": "1", "numTurns": "1" },
        },
    })
}

#[tokio::test]
async fn failed_first_turn_on_screenless_worker_is_classified_and_persisted() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-failed", "58960-58989").await;
    ctx.dispatch(&project, "c-failed", "58960-58989").await;

    // The print worker has no screen; the node even reports a stale "ready".
    ctx.node.set_print("ready");
    let instance_id = {
        let observed = ctx.observe().await;
        observed["items"][0]["instanceId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    ctx.node.append_journal(
        &instance_id,
        &[
            assistant_error_event("API Error: 400 requested model is not available"),
            turn_error_event(),
        ],
    );

    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(
        row["watch"]["status"], "failed",
        "a failed first turn must not read as working: {row}"
    );
    assert_eq!(
        row["watch"]["reason"],
        "API Error: 400 requested model is not available"
    );
    let detail = row["watch"]["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("screen-unavailable"),
        "detail must disclose the missing screen: {detail}"
    );
    // Failed is a watch-only state: the durable lifecycle is untouched.
    assert_eq!(row["state"]["state"], "working");
    assert!(row["watch"]["observedAt"].is_string(), "{row}");

    // The Hub instance row itself converged to failed from the journal.
    let (status, instance) = ctx
        .request("GET", &format!("/v1/instances/{instance_id}"), None)
        .await;
    assert_eq!(status, 200, "{instance}");
    assert_eq!(instance["lifecycle"], "failed");

    // Persisted on the roster, sticky on the next observation, with timestamp.
    let (_, roster) = ctx.request("GET", "/v1/workers", None).await;
    let persisted = roster["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == json!("c-failed"))
        .expect("roster row");
    assert_eq!(persisted["watch"]["status"], "failed");
    assert!(persisted["watch"]["observedAt"].is_string());
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "failed");
}

#[tokio::test]
async fn failed_carrier_lifecycle_classifies_without_journal() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-failed2", "58990-59019").await;
    ctx.dispatch(&project, "c-failed2", "58990-59019").await;

    // Dead pty carrier: no screen rows, terminal lifecycle failed.
    ctx.node.set_print("failed");
    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(row["watch"]["status"], "failed", "{row}");
    // No assistant text or driver error: the lifecycle reason code stands in.
    assert!(
        row["watch"]["reason"]
            .as_str()
            .unwrap_or("")
            .contains("instance-failed"),
        "{}",
        row["watch"]["reason"]
    );
    assert_eq!(row["state"]["state"], "working");
}

#[tokio::test]
async fn screenless_idle_after_api_error_does_not_overstate_failure() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-failed3", "59020-59049").await;
    ctx.dispatch(&project, "c-idleerr", "59020-59049").await;
    ctx.node.set_print("ready");
    let instance_id = {
        let observed = ctx.observe().await;
        observed["items"][0]["instanceId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Retrying, turn not yet errored: idle-api-error, not failed.
    ctx.node.append_journal(
        &instance_id,
        &[assistant_error_event(
            "API Error: upstream returned 502, Retrying…",
        )],
    );
    let observed = ctx.observe().await;
    assert_eq!(
        observed["items"][0]["watch"]["status"], "idle-api-error",
        "{}",
        observed["items"][0]
    );
    assert!(
        observed["items"][0]["watch"]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("screen-unavailable")
    );
}

#[tokio::test]
async fn recovered_error_then_done_before_exit_is_done_not_failed() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-failed4", "59050-59079").await;
    ctx.dispatch(&project, "c-recover", "59050-59079").await;
    ctx.node.set_print("ready");
    let instance_id = {
        let observed = ctx.observe().await;
        observed["items"][0]["instanceId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // The worker hit an API error, the retry succeeded, and its final message
    // is the DONE report before the process exits 0.
    ctx.node.append_journal(
        &instance_id,
        &[
            assistant_error_event("API Error: 429 rate limited, Retrying…"),
            json!({
                "kind": "message",
                "payload": {
                    "role": "assistant", "phase": "final",
                    "blocks": [{ "type": "text", "text": "DONE 0a10ebf351aa" }],
                },
            }),
            json!({
                "kind": "lifecycle",
                "payload": {
                    "type": "native", "topic": "session", "nativeName": "session",
                    "status": { "value": "exited" },
                },
            }),
        ],
    );
    ctx.node.set_print("exited");
    let observed = ctx.observe().await;
    let row = &observed["items"][0];
    assert_eq!(row["watch"]["status"], "done", "{row}");
    assert_eq!(row["watch"]["sha"], "0a10ebf351aa");
    assert_eq!(row["state"]["state"], "done");
}

// ── worker verbs ───────────────────────────────────────────────────────────

#[tokio::test]
async fn nudge_is_file_transport_and_throttled() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-nudge", "58750-58779").await;
    ctx.dispatch(&project, "c-nudge", "58750-58779").await;

    let (status, body) = ctx
        .request("POST", "/v1/workers/c-nudge/nudge", Some(json!({})))
        .await;
    assert_eq!(status, 200, "{body}");
    // Delivered as a file attachment on instance.send (never inline).
    let sends = ctx.node.payloads("instance.send");
    assert!(
        sends
            .iter()
            .flat_map(|payload| payload["attachments"]
                .as_array()
                .cloned()
                .unwrap_or_default())
            .any(|attachment| attachment["name"] == "nudge.md"),
        "nudge should be a file attachment: {sends:?}"
    );
    assert!(body["worker"]["lastNudgeAt"].is_string());

    // A second immediate nudge is throttled.
    let (status, _) = ctx
        .request("POST", "/v1/workers/c-nudge/nudge", Some(json!({})))
        .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn answer_sends_tty_write_keys() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-answer", "58780-58809").await;
    ctx.dispatch(&project, "c-answer", "58780-58809").await;
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/c-answer/answer",
            Some(json!({ "key": "enter" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let writes = ctx.node.payloads("tty.write");
    assert_eq!(writes.last().unwrap()["keys"], json!(["enter"]));
    assert_eq!(writes.last().unwrap()["dataBase64"], "DQ==");

    let (status, _) = ctx
        .request(
            "POST",
            "/v1/workers/c-answer/answer",
            Some(json!({ "key": "2" })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        ctx.node.payloads("tty.write").last().unwrap()["dataBase64"],
        "Mg=="
    );
}

#[tokio::test]
async fn switch_model_confirms_only_when_dialog_shown() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-switch", "58810-58839").await;
    ctx.dispatch(&project, "c-switch", "58810-58839").await;

    // Screen shows the confirmation dialog → the dance confirms.
    ctx.node
        .set_screen(&["Switch model? Press Enter to confirm"], "ready");
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/c-switch/switch-model",
            Some(json!({ "model": "model_hub/alt[1m]" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["confirmed"], true);
    assert_eq!(body["worker"]["model"], "model_hub/alt[1m]");
    let writes = ctx.node.payloads("tty.write");
    // esc ×2, "/model alt" + CR, confirming Enter.
    assert_eq!(writes.len(), 4, "{writes:?}");
    assert_eq!(writes.last().unwrap()["dataBase64"], "DQ==");

    // Without a confirmation dialog on screen → no confirming Enter, model
    // left unchanged (3 writes: esc, esc, "/model …").
    let project2 = project_with_enrolled_workspace(&ctx, "watch-switch2", "58840-58869").await;
    ctx.dispatch(&project2, "c-switch2", "58840-58869").await;
    ctx.node
        .set_screen(&["> /model model_hub/alt[1m]"], "ready");
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/c-switch2/switch-model",
            Some(json!({ "model": "model_hub/alt[1m]" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["confirmed"], false);
    assert_ne!(body["worker"]["model"], "model_hub/alt[1m]");
}

#[tokio::test]
async fn resume_relaunches_in_same_worktree_and_resends_brief() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-resume", "58870-58899").await;
    let (_, _) = ctx.dispatch(&project, "c-resume", "58870-58899").await;
    let before = ctx.observe().await;
    let old_instance = before["items"][0]["instanceId"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = ctx
        .request("POST", "/v1/workers/c-resume/resume", Some(json!({})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["mode"], "relaunch");
    let new_instance = body["instanceId"].as_str().unwrap().to_string();
    assert_ne!(old_instance, new_instance);
    assert_eq!(body["worker"]["instanceId"], new_instance);
    assert_eq!(body["worker"]["resumedFrom"], old_instance);
    assert_eq!(body["worker"]["state"]["state"], "working");

    // A fresh instance.create drove the respawn, and the brief was re-sent to
    // the NEW instance as a file handback.
    assert!(ctx.node.methods().contains(&"instance.create".to_string()));
    let handback = ctx
        .node
        .payloads("instance.send")
        .into_iter()
        .find(|payload| payload["instanceId"] == new_instance)
        .expect("handback send to the new instance");
    assert_eq!(handback["attachments"][0]["name"], "handback.md");
}

#[tokio::test]
async fn replace_retires_and_redispatches_same_brief() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-replace", "58900-58929").await;
    let (old_id, _) = ctx.dispatch(&project, "c-replace", "58900-58929").await;

    let (status, body) = ctx
        .request("POST", "/v1/workers/c-replace/replace", Some(json!({})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replaced"], old_id);
    assert_eq!(body["worker"]["name"], "c-replace");
    assert_eq!(body["worker"]["replaceCount"], "1");
    // The re-dispatch lands on a fresh branch slug (the old branch is kept).
    let branch = body["worker"]["branch"].as_str().unwrap();
    assert!(branch.starts_with("wt/c-replace/"));
    assert_ne!(body["worker"]["id"], old_id);
    assert!(ctx.node.methods().contains(&"worker.remove".to_string()));
    assert!(ctx.node.methods().contains(&"worker.provision".to_string()));
}

#[tokio::test]
async fn stop_closes_without_reclaiming() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-stop", "58930-58959").await;
    ctx.dispatch(&project, "c-stop", "58930-58959").await;
    let (status, body) = ctx
        .request("POST", "/v1/workers/c-stop/stop", Some(json!({})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(ctx.node.methods().contains(&"instance.close".to_string()));
    // Stop never reclaims the worktree/target dir.
    assert!(!ctx.node.methods().contains(&"worker.remove".to_string()));
}
