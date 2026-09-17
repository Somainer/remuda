//! Batch 6 (co-lanes) gate queue tests against the real Hub + scripted
//! lane-Node fakes: parallel verify across lanes, serialized land with the
//! compare-and-swap re-verify retry, FIFO queue order, cancel (queued and
//! running), and streamed step results.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// One scripted `gate.run`: optional events sent node→Hub first, a delay
/// (or a wait for `gate.cancel`), then the result reply.
#[derive(Clone)]
struct RunSpec {
    delay_ms: u64,
    wait_cancel: bool,
    events: Vec<Value>,
    status: &'static str,
    merge_sha: Option<&'static str>,
    base_sha: Option<&'static str>,
    current_main: Option<&'static str>,
    /// Ref the lane reports pinning the verified merge under.
    merge_ref: Option<&'static str>,
    error: Option<&'static str>,
    failed_step: Option<&'static str>,
    reason: Option<&'static str>,
    run_log: Option<Value>,
    steps: Option<Value>,
}

impl Default for RunSpec {
    fn default() -> Self {
        Self {
            delay_ms: 20,
            wait_cancel: false,
            events: Vec::new(),
            status: "passed",
            merge_sha: None,
            base_sha: Some("1111111111111111111111111111111111111111"),
            current_main: None,
            merge_ref: None,
            error: None,
            failed_step: None,
            reason: None,
            run_log: None,
            steps: None,
        }
    }
}

#[derive(Default)]
struct ScriptState {
    /// branch → successive run scripts.
    runs: Mutex<BTreeMap<String, VecDeque<RunSpec>>>,
    cancel_notify: Arc<Notify>,
    /// Observed gate.run params in arrival order.
    arrivals: Mutex<Vec<Value>>,
    gate_cancels: Mutex<Vec<Value>>,
    /// Observed gate.land params (the home host's push requests).
    land_calls: Mutex<Vec<Value>>,
    /// Successive `gate.land` verdicts; defaults to `landed`.
    land_replies: Mutex<VecDeque<Value>>,
    /// Observed gate.unpin params (ref cleanup).
    unpin_calls: Mutex<Vec<Value>>,
    /// Node→Hub request counter (ids must be unique).
    seq: Mutex<u64>,
}

impl ScriptState {
    fn script(&self, branch: &str, spec: RunSpec) {
        self.runs
            .lock()
            .unwrap()
            .entry(branch.to_owned())
            .or_default()
            .push_back(spec);
    }

    fn next_spec(&self, branch: &str) -> RunSpec {
        let mut runs = self.runs.lock().unwrap();
        if let Some(queue) = runs.get_mut(branch)
            && let Some(spec) = queue.pop_front()
        {
            return spec;
        }
        RunSpec::default()
    }
}

struct FakeLaneNode {
    _task: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    script: Arc<ScriptState>,
}

impl Drop for FakeLaneNode {
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

async fn enroll(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace_id: &str,
    script: Arc<ScriptState>,
) -> Result<FakeLaneNode> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {"hostId": host_id, "nodeVersion": "0.1.0-test",
                "label": format!("lane-{host_id}"),
                "host": {"hostname": format!("{host_id}.local"), "maxInstances": 8,
                    "labels": {}, "resources": {"cpuCount": 8},
                    "workspaces": [
                        {"workspaceId": workspace_id, "hostId": host_id,
                         "root": format!("/tmp/{workspace_id}")}
                    ],
                    "workspaceRevision": 1}}
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    assert!(hello["result"]["nodeToken"].is_string(), "{hello}");

    let script_for_task = script.clone();
    let task = tokio::spawn(async move {
        let script = script_for_task;
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            let result = match method {
                "gate.run" => {
                    script.arrivals.lock().unwrap().push(params.clone());
                    run_one_inline(&script, &params, &mut node).await
                }
                "gate.cancel" => {
                    let job = params["jobId"].as_str().unwrap_or("").to_string();
                    script.gate_cancels.lock().unwrap().push(params);
                    script.cancel_notify.notify_waiters();
                    json!({"ok": true, "jobId": job})
                }
                "gate.land" => {
                    let job = params["jobId"].as_str().unwrap_or("").to_string();
                    script.land_calls.lock().unwrap().push(params);
                    script
                        .land_replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| {
                            json!({"jobId": job, "status": "landed",
                                   "mergeSha": "3333333333333333333333333333333333333333"})
                        })
                }
                "gate.unpin" => {
                    let job = params["jobId"].as_str().unwrap_or("").to_string();
                    script.unpin_calls.lock().unwrap().push(params);
                    json!({"jobId": job, "removed": [
                        format!("refs/remuda/gate/{job}"),
                        format!("refs/remuda/gate/{job}.branch"),
                    ]})
                }
                _ => json!({"ok": true}),
            };
            let response = json!({"jsonrpc":"2.0","id":id,"result":result});
            if node
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(FakeLaneNode {
        _task: task,
        script,
    })
}

fn result_json(job_id: &str, spec: &RunSpec) -> Value {
    let mut result = json!({
        "jobId": job_id,
        "status": spec.status,
        "baseSha": spec.base_sha,
        "mergeSha": spec.merge_sha,
        "currentMainSha": spec.current_main,
        "error": spec.error,
        "failedStep": spec.failed_step,
        "reason": spec.reason,
        "runLog": spec.run_log,
        "steps": spec.steps,
    });
    // Emit absent fields like a real Node (skip defaults).
    if let Some(object) = result.as_object_mut() {
        if let Some(merge_ref) = spec.merge_ref {
            object.insert("mergeRef".into(), json!(merge_ref));
        }
        if spec.failed_step.is_none() {
            object.remove("failedStep");
        }
        if spec.reason.is_none() {
            object.remove("reason");
        }
        if spec.run_log.is_none() {
            object.remove("runLog");
        }
        if spec.steps.is_none() {
            object.remove("steps");
        }
    }
    result
}

/// A connected lane Node socket.
type LaneSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Run a scripted gate job inline. While waiting, keep draining the socket so
/// a concurrent `gate.cancel` for this job is observed (and any other RPC
/// still gets its ack). Returns the gate.run result.
async fn run_one_inline(script: &Arc<ScriptState>, params: &Value, node: &mut LaneSocket) -> Value {
    let job_id = params["jobId"].as_str().unwrap_or("").to_string();
    let branch = params["branch"].as_str().unwrap_or("").to_string();
    let spec = script.next_spec(&branch);

    // Send streamed events first.
    for event in &spec.events {
        let event_id = {
            let mut seq = script.seq.lock().unwrap();
            *seq += 1;
            format!("evt-{seq}")
        };
        let mut event_params = json!({ "jobId": job_id });
        if let Some(object) = event.as_object() {
            for (key, value) in object {
                event_params[key] = value.clone();
            }
        }
        let frame = json!({
            "jsonrpc": "2.0", "id": event_id,
            "method": "gate.event", "params": event_params,
        });
        let _ = node.send(Message::Text(frame.to_string().into())).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Wait, pumping inbound RPCs (notably gate.cancel) the whole time.
    let deadline = (!spec.wait_cancel).then(|| Duration::from_millis(spec.delay_ms));
    loop {
        tokio::select! {
            _ = tokio::time::sleep(deadline.unwrap_or(Duration::MAX)), if deadline.is_some() => {
                break;
            }
            frame = node.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let frame: Value = serde_json::from_str(&text).unwrap_or(json!({}));
                        let Some(id) = frame.get("id").cloned() else { continue };
                        let Some(method) = frame.get("method").and_then(Value::as_str) else {
                            continue;
                        };
                        let in_params = frame.get("params").cloned().unwrap_or(json!({}));
                        if method == "gate.cancel"
                            && in_params["jobId"].as_str() == Some(job_id.as_str())
                        {
                            script.gate_cancels.lock().unwrap().push(in_params);
                            script.cancel_notify.notify_waiters();
                            let reply = json!({"jsonrpc":"2.0","id":id,"result":{"ok":true,"jobId":job_id}});
                            let _ = node.send(Message::Text(reply.to_string().into())).await;
                            if spec.wait_cancel {
                                break;
                            }
                        } else {
                            let reply = json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}});
                            let _ = node.send(Message::Text(reply.to_string().into())).await;
                        }
                    }
                    other => eprintln!("fake node socket event mid-run: {other:?}"),
                }
            }
        }
    }
    result_json(&job_id, &spec)
}

// ── test harness ───────────────────────────────────────────────────────────

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    token: String,
    hosts: Vec<String>,
    project: String,
    scripts: Vec<Arc<ScriptState>>,
    _nodes: Vec<FakeLaneNode>,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
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
            .bearer_auth(&self.token);
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(json!(null));
        (status, body)
    }

    async fn enqueue(&self, body: Value) -> Value {
        let (status, value) = self
            .request(
                "POST",
                &format!("/v1/projects/{}/gate", self.project),
                Some(body),
            )
            .await;
        assert!(status.is_success(), "enqueue {status}: {value}");
        value
    }

    async fn job(&self, id: &str) -> Value {
        let (status, value) = self
            .request(
                "GET",
                &format!("/v1/projects/{}/gate/jobs/{id}", self.project),
                None,
            )
            .await;
        assert_eq!(status, 200, "get job {value}");
        value
    }

    async fn wait_state(&self, id: &str, states: &[&str]) -> Value {
        for _ in 0..100 {
            let job = self.job(id).await;
            if states
                .iter()
                .any(|wanted| job["state"].as_str() == Some(*wanted))
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {id} never reached {states:?}: {}", self.job(id).await);
    }

    async fn spawn(lanes: usize) -> Result<Ctx> {
        Self::spawn_with(lanes, |_| {}).await
    }

    async fn spawn_with(lanes: usize, configure: impl FnOnce(&mut Value)) -> Result<Ctx> {
        Self::spawn_with_config(lanes, configure, |_| {}).await
    }

    async fn spawn_with_config(
        lanes: usize,
        configure: impl FnOnce(&mut Value),
        tune: impl FnOnce(&mut HubConfig),
    ) -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let mut config = HubConfig::for_test(dir.path().join("hub"));
        tune(&mut config);
        let hub = spawn(config).await?;
        let token = hub.mint_device_token("gate-human").await?;

        let mut host_ids = Vec::new();
        let mut nodes = Vec::new();
        let mut lane_docs = Vec::new();
        let mut members = Vec::new();
        let mut host_quotas = Vec::new();
        let mut scripts = Vec::new();
        for index in 0..lanes {
            let host_id = remuda_protocol::HostId::new().as_id().to_string();
            let workspace_id = remuda_protocol::WorkspaceId::new().as_id().to_string();
            let script = Arc::new(ScriptState {
                cancel_notify: Arc::new(Notify::new()),
                ..Default::default()
            });
            let node = enroll(&hub, &host_id, &workspace_id, script.clone()).await?;
            nodes.push(node);
            scripts.push(script);
            lane_docs.push(json!({
                "id": format!("lane{}", index + 1),
                "hostId": host_id,
                "repoPath": format!("/tmp/lane{}/repo", index + 1),
                "targetDir": format!("/tmp/lane{}/target", index + 1),
                "ports": format!("584{}0-584{}9", index, index),
                "env": {"CARGO_BUILD_JOBS": "8"},
                "lockPath": "/tmp/remuda-agents/e2e.lock",
                "pwEndpoint": "ws://127.0.0.1:3177/",
            }));
            members
                .push(json!({ "hostId": host_id, "workspaceId": workspace_id, "role": "build" }));
            host_quotas.push(json!({ "hostId": host_id, "maxInstances": 8 }));
            host_ids.push(host_id);
        }
        // Let hello settle so the scheduler sees the Nodes online.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let http = reqwest::Client::new();
        let mut project_body = json!({
            "name": "gate-project",
            "members": members,
            "hosts": host_quotas,
            "gate": {
                "affected": true, "web": "auto",
                "landSerialization": "global-cas",
                "lanes": lane_docs,
            },
        });
        configure(&mut project_body);
        let response = http
            .post(format!("http://{}/v1/projects", hub.addr))
            .bearer_auth(&token)
            .json(&project_body)
            .send()
            .await?;
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
        let created: Value = response.json().await?;
        let project = created["id"].as_str().unwrap().to_owned();

        Ok(Ctx {
            _dir: dir,
            hub,
            http,
            token,
            hosts: host_ids,
            project,
            scripts,
            _nodes: nodes,
        })
    }
}

fn step(name: &str, status: &str) -> Value {
    json!({"kind":"step","step":{"name":name,"status":status,"durationMs":12}})
}

// ── tests ──────────────────────────────────────────────────────────────────

/// A passing verify records the lane's pinned merge ref on the job, so a later
/// land (or an operator) can find the merge commit that survived the worktree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passing_verify_records_the_merge_ref_on_the_job() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    ctx.scripts[0].script(
        "wt/a/task",
        RunSpec {
            merge_sha: Some("3333333333333333333333333333333333333333"),
            merge_ref: Some("refs/remuda/gate/pinned"),
            ..RunSpec::default()
        },
    );
    let job = ctx
        .enqueue(json!({"branch":"wt/a/task","mode":"verify"}))
        .await;
    let id = job["id"].as_str().unwrap();
    let final_job = ctx.wait_state(id, &["passed", "failed"]).await;
    assert_eq!(final_job["state"], "passed", "{final_job}");
    assert_eq!(
        final_job["mergeRef"], "refs/remuda/gate/pinned",
        "mergeRef is returned on the job: {final_job}"
    );
    assert_eq!(
        final_job["mergeSha"], "3333333333333333333333333333333333333333",
        "{final_job}"
    );
    Ok(())
}

/// With `pushFrom: home` the lane verifies and the project home host pushes.
/// The job only reaches `landed` after that push, and the lane's refs are then
/// dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_from_home_lands_via_the_home_host_then_unpins() -> Result<()> {
    // Two lanes: lane1 is the credential-less build lane, lane2 is the home
    // host's own checkout (the project homeHost is lane2's host).
    let ctx = Ctx::spawn_with(2, |body| {
        body["gate"]["lanes"][0]["pushFrom"] = json!("home");
        body["gate"]["lanes"][0]["fetchRemote"] = json!("lane-alias");
    })
    .await?;
    // Pin the job to lane1 and make lane2's host the home host.
    let response = ctx
        .http
        .patch(format!(
            "http://{}/v1/projects/{}",
            ctx.hub.addr, ctx.project
        ))
        .bearer_auth(&ctx.token)
        .json(&json!({"homeHost": ctx.hosts[1]}))
        .send()
        .await?;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);

    ctx.scripts[0].script(
        "wt/a/task",
        RunSpec {
            // A `pushFrom: home` lane reports a *pass* for a land job.
            status: "passed",
            merge_sha: Some("3333333333333333333333333333333333333333"),
            merge_ref: Some("refs/remuda/gate/pinned"),
            ..RunSpec::default()
        },
    );
    let job = ctx
        .enqueue(json!({"branch":"wt/a/task","mode":"land","laneId":"lane1"}))
        .await;
    let id = job["id"].as_str().unwrap();
    let final_job = ctx.wait_state(id, &["landed", "failed", "passed"]).await;
    assert_eq!(
        final_job["state"], "landed",
        "the home host's push completes the land: {final_job}"
    );

    // The home host (lane2's host), not the lane, was asked to push.
    let land_calls = ctx.scripts[1].land_calls.lock().unwrap().clone();
    assert_eq!(land_calls.len(), 1, "exactly one push: {land_calls:?}");
    assert_eq!(land_calls[0]["mergeRef"], "refs/remuda/gate/pinned");
    assert_eq!(
        land_calls[0]["mergeSha"],
        "3333333333333333333333333333333333333333"
    );
    assert_eq!(
        land_calls[0]["fetchRemote"], "lane-alias",
        "the lane's fetchRemote is what the home host fetches over"
    );
    assert_eq!(
        land_calls[0]["baseSha"], "1111111111111111111111111111111111111111",
        "the CAS guard is the verified base"
    );
    assert!(
        ctx.scripts[0].land_calls.lock().unwrap().is_empty(),
        "the credential-less lane must never be asked to push"
    );

    // The lane never ran --land: its gate.run said pushFrom home.
    let arrivals = ctx.scripts[0].arrivals.lock().unwrap().clone();
    assert_eq!(arrivals[0]["pushFrom"], "home", "{arrivals:?}");

    // A completed land drops the pinned refs on the lane host.
    for _ in 0..40 {
        if !ctx.scripts[0].unpin_calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let unpins = ctx.scripts[0].unpin_calls.lock().unwrap().clone();
    assert_eq!(unpins.len(), 1, "the lane's refs are dropped: {unpins:?}");
    assert_eq!(unpins[0]["jobId"], id);
    assert_eq!(unpins[0]["repoPath"], "/tmp/lane1/repo");
    Ok(())
}

/// A home-host CAS refusal must re-queue a verify, never push and never claim
/// success. The re-verify then lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn home_land_base_moved_requeues_a_verify_instead_of_pushing() -> Result<()> {
    let ctx = Ctx::spawn_with(2, |body| {
        body["gate"]["lanes"][0]["pushFrom"] = json!("home");
        body["gate"]["lanes"][0]["fetchRemote"] = json!("lane-alias");
    })
    .await?;
    let response = ctx
        .http
        .patch(format!(
            "http://{}/v1/projects/{}",
            ctx.hub.addr, ctx.project
        ))
        .bearer_auth(&ctx.token)
        .json(&json!({"homeHost": ctx.hosts[1]}))
        .send()
        .await?;
    assert_eq!(response.status(), 200);

    // First push is refused: main moved under us. Second succeeds.
    {
        let mut replies = ctx.scripts[1].land_replies.lock().unwrap();
        replies.push_back(json!({
            "jobId": "", "status": "base-moved",
            "currentMainSha": "9999999999999999999999999999999999999999",
        }));
        replies.push_back(json!({
            "jobId": "", "status": "landed",
            "mergeSha": "4444444444444444444444444444444444444444",
        }));
    }
    // Two lane verifies: the original and the re-verify onto the new base.
    for merge in [
        "3333333333333333333333333333333333333333",
        "4444444444444444444444444444444444444444",
    ] {
        ctx.scripts[0].script(
            "wt/a/task",
            RunSpec {
                status: "passed",
                merge_sha: Some(merge),
                merge_ref: Some("refs/remuda/gate/pinned"),
                ..RunSpec::default()
            },
        );
    }

    let job = ctx
        .enqueue(json!({"branch":"wt/a/task","mode":"land","laneId":"lane1"}))
        .await;
    let id = job["id"].as_str().unwrap();
    let final_job = ctx.wait_state(id, &["landed", "failed"]).await;
    assert_eq!(
        final_job["state"], "landed",
        "the re-verify lands after the refusal: {final_job}"
    );
    // The refusal cost one extra verify attempt, and the merge that landed is
    // the re-verified one — never the stale merge the CAS refused.
    assert_eq!(
        ctx.scripts[0].arrivals.lock().unwrap().len(),
        2,
        "a moved base re-queues a verify"
    );
    assert_eq!(
        final_job["mergeSha"], "4444444444444444444444444444444444444444",
        "{final_job}"
    );
    assert_eq!(
        ctx.scripts[1].land_calls.lock().unwrap().len(),
        2,
        "one refused push, one that landed"
    );
    Ok(())
}

/// Retention drops the pinned refs of a passing verify nobody landed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ref_retention_drops_pins_for_an_unlanded_verify() -> Result<()> {
    // A one-second window: long enough to observe the job holding its ref,
    // short enough for the sweep to expire it inside the test.
    let ctx = Ctx::spawn_with_config(
        1,
        |body| {
            body["gate"]["lanes"][0]["fetchRemote"] = json!("lane-alias");
        },
        |config| config.gate_ref_retention_ms = 1_000,
    )
    .await?;
    ctx.scripts[0].script(
        "wt/a/task",
        RunSpec {
            merge_sha: Some("3333333333333333333333333333333333333333"),
            merge_ref: Some("refs/remuda/gate/pinned"),
            ..RunSpec::default()
        },
    );
    let job = ctx
        .enqueue(json!({"branch":"wt/a/task","mode":"verify"}))
        .await;
    let id = job["id"].as_str().unwrap();
    let passed = ctx.wait_state(id, &["passed", "failed"]).await;
    assert_eq!(passed["state"], "passed", "{passed}");
    assert_eq!(
        passed["mergeRef"], "refs/remuda/gate/pinned",
        "the ref is held while the verify is still landable: {passed}"
    );

    // The sweep runs on the scheduler tick; wait for the unpin call.
    let mut swept = false;
    for _ in 0..80 {
        if !ctx.scripts[0].unpin_calls.lock().unwrap().is_empty() {
            swept = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(swept, "retention must drop an unlanded verify's refs");
    let unpins = ctx.scripts[0].unpin_calls.lock().unwrap().clone();
    assert_eq!(unpins[0]["jobId"], id, "{unpins:?}");
    assert_eq!(unpins[0]["repoPath"], "/tmp/lane1/repo", "{unpins:?}");
    // The job keeps its mergeSha as evidence but no longer claims a live ref.
    let after = ctx.job(id).await;
    assert!(
        after["mergeRef"].is_null(),
        "a swept job must not advertise a dropped ref: {after}"
    );
    assert_eq!(
        after["mergeSha"], "3333333333333333333333333333333333333333",
        "the sha stays as evidence of what was verified: {after}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verify_job_runs_on_a_lane_and_passes() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    ctx.scripts[0].script(
        "wt/a/task",
        RunSpec {
            events: vec![step("cargo-fmt", "ok"), step("cargo-test", "ok")],
            ..RunSpec::default()
        },
    );
    let job = ctx
        .enqueue(json!({"branch":"wt/a/task","mode":"verify"}))
        .await;
    let id = job["id"].as_str().unwrap();
    let final_job = ctx.wait_state(id, &["passed", "failed"]).await;
    assert_eq!(final_job["state"], "passed", "{final_job}");
    assert_eq!(final_job["laneId"], "lane1");
    let names: Vec<&str> = final_job["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"cargo-fmt"), "{names:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_verify_jobs_run_in_parallel_on_two_lanes() -> Result<()> {
    let ctx = Ctx::spawn(2).await?;
    // Both runs linger long enough for the second dispatch to be observed.
    for index in 0..2 {
        ctx.scripts[index].script(
            if index == 0 { "wt/a/one" } else { "wt/b/two" },
            RunSpec {
                delay_ms: 1500,
                ..RunSpec::default()
            },
        );
    }
    let a = ctx.enqueue(json!({"branch":"wt/a/one"})).await;
    let b = ctx.enqueue(json!({"branch":"wt/b/two"})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let a_running = ctx
        .wait_state(a["id"].as_str().unwrap(), &["running"])
        .await;
    let b_running = ctx
        .wait_state(b["id"].as_str().unwrap(), &["running"])
        .await;
    assert_eq!(a_running["hostId"].as_str(), Some(ctx.hosts[0].as_str()));
    assert_eq!(b_running["hostId"].as_str(), Some(ctx.hosts[1].as_str()));
    // Different lanes/hosts, simultaneously.
    assert_ne!(a_running["laneId"], b_running["laneId"]);
    for job in [&a, &b] {
        let done = ctx
            .wait_state(job["id"].as_str().unwrap(), &["passed"])
            .await;
        assert_eq!(done["state"], "passed");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn land_jobs_are_serialized_and_the_second_lands_after() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    ctx.scripts[0].script(
        "wt/a/land",
        RunSpec {
            delay_ms: 1500,
            status: "landed",
            merge_sha: Some("2222222222222222222222222222222222222222"),
            ..RunSpec::default()
        },
    );
    ctx.scripts[0].script(
        "wt/b/land",
        RunSpec {
            delay_ms: 100,
            status: "landed",
            merge_sha: Some("3333333333333333333333333333333333333333"),
            ..RunSpec::default()
        },
    );
    let a = ctx
        .enqueue(json!({"branch":"wt/a/land","mode":"land"}))
        .await;
    let b = ctx
        .enqueue(json!({"branch":"wt/b/land","mode":"land"}))
        .await;
    // While A is landing, B stays queued.
    let _ = ctx
        .wait_state(a["id"].as_str().unwrap(), &["running"])
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(ctx.job(b["id"].as_str().unwrap()).await["state"], "queued");
    let a_done = ctx.wait_state(a["id"].as_str().unwrap(), &["landed"]).await;
    assert_eq!(
        a_done["mergeSha"],
        "2222222222222222222222222222222222222222"
    );
    // Then B gets its serial turn and lands.
    let b_done = ctx
        .wait_state(b["id"].as_str().unwrap(), &["landed", "failed"])
        .await;
    assert_eq!(b_done["state"], "landed", "{b_done}");
    assert_eq!(
        b_done["mergeSha"],
        "3333333333333333333333333333333333333333"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn land_losing_the_cas_is_reverified_onto_new_main_then_lands() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    // First attempt: base moved. Second attempt (re-queue) lands.
    ctx.scripts[0].script(
        "wt/a/cas",
        RunSpec {
            delay_ms: 50,
            status: "base-moved",
            base_sha: Some("1111111111111111111111111111111111111111"),
            current_main: Some("9999999999999999999999999999999999999999"),
            error: Some("main moved since verification"),
            ..RunSpec::default()
        },
    );
    ctx.scripts[0].script(
        "wt/a/cas",
        RunSpec {
            delay_ms: 50,
            status: "landed",
            base_sha: Some("9999999999999999999999999999999999999999"),
            merge_sha: Some("4444444444444444444444444444444444444444"),
            ..RunSpec::default()
        },
    );
    let job = ctx
        .enqueue(json!({"branch":"wt/a/cas","mode":"land"}))
        .await;
    let id = job["id"].as_str().unwrap();
    let done = ctx.wait_state(id, &["landed", "failed"]).await;
    assert_eq!(done["state"], "landed", "{done}");
    assert!(
        done["attempts"].as_u64().unwrap() >= 2,
        "{}",
        done["attempts"]
    );
    assert_eq!(done["mergeSha"], "4444444444444444444444444444444444444444");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifo_order_is_preserved_on_one_lane() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    // A lingers; B and C must queue behind it in that order. B holds until
    // canceled (rather than racing a fixed delay) so the "C still queued
    // behind a running B" assertion is deterministic under host load.
    ctx.scripts[0].script(
        "wt/a/fifo",
        RunSpec {
            delay_ms: 1500,
            ..RunSpec::default()
        },
    );
    ctx.scripts[0].script(
        "wt/b/fifo",
        RunSpec {
            wait_cancel: true,
            status: "canceled",
            ..RunSpec::default()
        },
    );
    ctx.scripts[0].script(
        "wt/c/fifo",
        RunSpec {
            delay_ms: 200,
            ..RunSpec::default()
        },
    );
    let a = ctx.enqueue(json!({"branch":"wt/a/fifo"})).await;
    let b = ctx.enqueue(json!({"branch":"wt/b/fifo"})).await;
    let c = ctx.enqueue(json!({"branch":"wt/c/fifo"})).await;
    let _ = ctx
        .wait_state(a["id"].as_str().unwrap(), &["running"])
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ctx.job(b["id"].as_str().unwrap()).await["state"], "queued");
    assert_eq!(ctx.job(c["id"].as_str().unwrap()).await["state"], "queued");
    let _ = ctx.wait_state(a["id"].as_str().unwrap(), &["passed"]).await;
    // B must be running while C is still queued — B holds until canceled, so
    // this ordering cannot be missed between two scheduler polls.
    let _ = ctx
        .wait_state(b["id"].as_str().unwrap(), &["running"])
        .await;
    assert_eq!(ctx.job(c["id"].as_str().unwrap()).await["state"], "queued");
    // Release B; C starts only after B's dispatch ended.
    let (status, _) = ctx
        .request(
            "POST",
            &format!(
                "/v1/projects/{}/gate/jobs/{}/cancel",
                ctx.project,
                b["id"].as_str().unwrap()
            ),
            Some(json!({})),
        )
        .await;
    assert!(status.is_success());
    let _ = ctx
        .wait_state(b["id"].as_str().unwrap(), &["canceled"])
        .await;
    let _ = ctx.wait_state(c["id"].as_str().unwrap(), &["passed"]).await;
    // Arrival order at the Node (B's gate.run was dispatched even though it
    // was canceled while running).
    let arrivals = ctx.scripts[0].arrivals.lock().unwrap();
    let branches: Vec<&str> = arrivals
        .iter()
        .map(|params| params["branch"].as_str().unwrap())
        .collect();
    assert_eq!(branches, vec!["wt/a/fifo", "wt/b/fifo", "wt/c/fifo"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_a_queued_job_marks_it_canceled_without_a_node_call() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    ctx.scripts[0].script(
        "wt/a/hold",
        RunSpec {
            delay_ms: 1500,
            ..RunSpec::default()
        },
    );
    ctx.scripts[0].script(
        "wt/b/hold",
        RunSpec {
            delay_ms: 50,
            ..RunSpec::default()
        },
    );
    let _a = ctx.enqueue(json!({"branch":"wt/a/hold"})).await;
    let b = ctx.enqueue(json!({"branch":"wt/b/hold"})).await;
    let bid = b["id"].as_str().unwrap();
    let _ = ctx
        .wait_state(_a["id"].as_str().unwrap(), &["running"])
        .await;
    let (status, canceled) = ctx
        .request(
            "POST",
            &format!("/v1/projects/{}/gate/jobs/{bid}/cancel", ctx.project),
            Some(json!({})),
        )
        .await;
    assert!(status.is_success(), "{canceled}");
    assert_eq!(canceled["state"], "canceled");
    let seen = ctx.scripts[0]
        .arrivals
        .lock()
        .unwrap()
        .iter()
        .any(|params| params["branch"] == "wt/b/hold");
    assert!(!seen, "canceled job was never dispatched");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_a_running_job_sends_gate_cancel_and_finishes_canceled() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    ctx.scripts[0].script(
        "wt/a/cancel",
        RunSpec {
            wait_cancel: true,
            status: "canceled",
            error: Some("canceled by request; killed step process group"),
            ..RunSpec::default()
        },
    );
    let job = ctx.enqueue(json!({"branch":"wt/a/cancel"})).await;
    let id = job["id"].as_str().unwrap();
    let _ = ctx.wait_state(id, &["running"]).await;
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/projects/{}/gate/jobs/{id}/cancel", ctx.project),
            Some(json!({})),
        )
        .await;
    assert!(status.is_success());
    let done = ctx.wait_state(id, &["canceled", "failed"]).await;
    assert_eq!(done["state"], "canceled", "{done}");
    assert!(!ctx.scripts[0].gate_cancels.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_job_persists_evidence_fields_and_a_fetchable_log_object() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    let run_log = json!({
        "step": "cargo-test",
        "kind": "failed",
        "attempts": 2,
        "headline": "cargo-test failed, exit status 101, attempts 2, retried",
        "summary": [
            "failures:",
            "    remuda::gate::boom",
            "test remuda::gate::boom ... FAILED",
            "thread 'remuda::gate::boom' panicked at crates/remuda-node/src/gate.rs:42:9:"
        ],
        "tail": [
            "test result: FAILED. 0 passed; 1 failed",
            "error: test failed, to rerun pass `-p remuda-node --lib gate`"
        ],
        "capturedLines": 84,
        "truncated": false,
    });
    ctx.scripts[0].script(
        "wt/a/broken",
        RunSpec {
            delay_ms: 50,
            status: "failed",
            error: Some("gate failed or returned an incomplete step report"),
            failed_step: Some("cargo-test"),
            reason: Some("cargo-test failed, exit status 101, attempts 2, retried"),
            run_log: Some(run_log),
            steps: Some(json!([
                {"name": "secret-scan", "status": "ok", "durationMs": 11},
                {"name": "cargo-test", "status": "failed", "durationMs": 5012,
                 "attempts": 2, "retried": true, "error": "exit status 101"}
            ])),
            ..RunSpec::default()
        },
    );
    let job = ctx
        .enqueue(json!({"branch":"wt/a/broken","mode":"verify"}))
        .await;
    let id = job["id"].as_str().unwrap().to_owned();
    let failed = ctx.wait_state(&id, &["failed"]).await;
    assert_eq!(failed["failedStep"], "cargo-test", "{failed}");
    assert_eq!(
        failed["reason"],
        "cargo-test failed, exit status 101, attempts 2, retried"
    );
    let object_id = failed["logObjectId"]
        .as_str()
        .expect("failed job names a log object")
        .to_owned();
    assert!(object_id.starts_with("obj_"), "{object_id}");

    // Fetch the log through the job id (what the CLI prints).
    let (status, by_job) = ctx
        .request("GET", &format!("/v1/gate/logs/{id}"), None)
        .await;
    assert_eq!(status, 200, "{by_job}");
    assert_eq!(by_job["jobId"], id);
    assert_eq!(by_job["objectId"], object_id);
    assert_eq!(by_job["log"]["step"], "cargo-test");
    assert_eq!(by_job["log"]["attempts"], 2);
    let summary = by_job["log"]["summary"].as_array().unwrap();
    assert!(
        summary
            .iter()
            .any(|line| line.as_str().unwrap().contains("FAILED"))
    );
    assert!(
        summary
            .iter()
            .any(|line| line.as_str().unwrap().contains("panicked at"))
    );
    assert!(by_job["expiresAt"].as_str().unwrap().len() > 10);

    // The same envelope is addressable by the obj_ id directly.
    let (status, by_object) = ctx
        .request("GET", &format!("/v1/gate/logs/{object_id}"), None)
        .await;
    assert_eq!(status, 200, "{by_object}");
    assert_eq!(by_object["log"]["headline"], by_job["log"]["headline"]);

    // Bad ids are rejected, missing jobs 404, another id prefix 400.
    let (status, _) = ctx.request("GET", "/v1/gate/logs/not-an-id", None).await;
    assert_eq!(status, 400);
    let (status, _) = ctx
        .request("GET", "/v1/gate/logs/gjb_doesnotexist", None)
        .await;
    assert_eq!(status, 404);

    // Passing runs stay cheap: no log object on a green job.
    ctx.scripts[0].script(
        "wt/a/green",
        RunSpec {
            delay_ms: 20,
            ..RunSpec::default()
        },
    );
    let green = ctx.enqueue(json!({"branch":"wt/a/green"})).await;
    let green_done = ctx
        .wait_state(green["id"].as_str().unwrap(), &["passed"])
        .await;
    assert!(green_done["logObjectId"].is_null(), "{green_done}");
    assert!(green_done["failedStep"].is_null());
    let (status, _) = ctx
        .request(
            "GET",
            &format!("/v1/gate/logs/{}", green["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, 404);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_lane_and_bad_inputs_are_rejected() -> Result<()> {
    let ctx = Ctx::spawn(1).await?;
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/projects/{}/gate", ctx.project),
            Some(json!({"branch":"wt/a/x","laneId":"nope"})),
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/projects/{}/gate", ctx.project),
            Some(json!({"branch":"main"})),
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/projects/{}/gate", ctx.project),
            Some(json!({"branch":"wt/a/x","mode":"land","web":"sometimes"})),
        )
        .await;
    assert_eq!(status, 400);
    Ok(())
}
