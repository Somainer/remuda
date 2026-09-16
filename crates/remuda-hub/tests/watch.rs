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
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Scripted screen the fake Node serves to `tty.screen`.
#[derive(Clone)]
struct ScreenState {
    supported: bool,
    lifecycle: String,
    lines: Vec<String>,
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
            supported: true,
            lifecycle: "ready".to_string(),
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
                            "source": "emulator",
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
    })
}

impl FakeNode {
    fn set_screen(&self, lines: &[&str], lifecycle: &str) {
        *self.screen.lock().unwrap() = ScreenState {
            supported: true,
            lifecycle: lifecycle.to_string(),
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
        };
    }

    fn set_gone(&self) {
        *self.screen.lock().unwrap() = ScreenState {
            supported: false,
            lifecycle: "closed".to_string(),
            lines: Vec::new(),
        };
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

#[tokio::test]
async fn gone_when_carrier_reports_closed() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = project_with_enrolled_workspace(&ctx, "watch-gone", "58720-58749").await;
    ctx.dispatch(&project, "c-gone", "58720-58749").await;
    ctx.node.set_gone();
    let observed = ctx.observe().await;
    assert_eq!(observed["items"][0]["watch"]["status"], "gone");
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
