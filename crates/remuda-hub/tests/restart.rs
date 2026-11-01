//! D-057 §6.3 / §6.3.1 / §6.4 (ma-restart): the C1 supervisor, the restart
//! notice and the C1 pushes, against fake Nodes with deterministic hooks and
//! no millisecond thresholds.
//!
//! Each §6.3 cause-table row has a test here; the supervisor runs through the
//! test handle (`test_supervise`, one unguarded evaluation) except where a
//! test is about the tick or the boot evaluation themselves.
#![allow(missing_docs)]

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8000-0000000000c1";
const EARLIER_SESSION: &str = "01993ab0-0000-7000-8000-0000000000c0";

/// Scripted `lineage.fence` acknowledgements: `(opId, outcome)` pairs, one
/// list per delivery.
type AckScript = Arc<Mutex<std::collections::VecDeque<Vec<(String, String)>>>>;

/// `(failed chapter, cause, outcome, successor)` of one decision row.
type DecisionRow = (String, String, String, Option<String>);

/// A fake Node: records every Hub→Node frame, acknowledges fences (with
/// scripted outcomes when queued), accepts everything else.
struct FakeNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    /// Scripted `lineage.fence` acknowledgements, one per delivery.
    #[allow(dead_code)]
    acks: AckScript,
    node_token: Option<String>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl FakeNode {
    async fn connect(
        hub: &remuda_hub::RunningHub,
        host: &str,
        fence_capable: bool,
    ) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        Self::connect_with(hub, host, &enroll, fence_capable, json!({})).await
    }

    async fn connect_with(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        fence_capable: bool,
        extra: Value,
    ) -> Result<Self> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut params = json!({ "hostId": host, "host": {
            "hostname": format!("{host}.restart"), "workspaces": [], "workspaceRevision": 0,
            "herdr": {"path": "/usr/bin/herdr"},
            "cli": [{"kind": "claude", "path": "/usr/bin/claude", "auth": "logged_in"}]
        }});
        if fence_capable {
            params["capabilities"] = json!({ "lineageFence": true });
        }
        if let (Some(object), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        node.send(Message::Text(
            json!({"jsonrpc":"2.0","id":"hello","method":"node.hello","params":params})
                .to_string()
                .into(),
        ))
        .await?;
        let hello: Value =
            serde_json::from_str(&node.next().await.context("hello")??.into_text()?)?;
        assert!(hello.get("result").is_some(), "hello failed: {hello}");
        let node_token = hello["result"]["nodeToken"].as_str().map(str::to_owned);
        let acks: AckScript = Arc::default();
        let (frame_tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let (appends, mut append_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
        let task_acks = Arc::clone(&acks);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((instance_id, event)) = append_rx.recv() => {
                        let frame = json!({"jsonrpc":"2.0","id":"append","method":"journal.append",
                            "params":{"instanceId":instance_id,"event":event}});
                        if node.send(Message::Text(frame.to_string().into())).await.is_err() { break; }
                    }
                    frame = node.next() => {
                        let Some(Ok(Message::Text(text))) = frame else {
                            if matches!(frame, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { break; }
                            continue;
                        };
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                        let Some(method) = frame["method"].as_str().map(str::to_owned) else { continue };
                        let id = frame["id"].clone();
                        let params = frame["params"].clone();
                        if frame_tx.send((method.clone(), params.clone())).is_err() { break; }
                        if id.is_null() { continue; }
                        let reply = if method == "lineage.fence" {
                            let outcomes = task_acks
                                .lock()
                                .unwrap()
                                .pop_front()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|(op, outcome)| json!({"opId": op, "outcome": outcome}))
                                .collect::<Vec<_>>();
                            json!({"jsonrpc":"2.0","id":id,"result":{
                                "fenceId": params["fenceId"], "closedInstanceIds": [], "outcomes": outcomes}})
                        } else {
                            json!({"jsonrpc":"2.0","id":id,"result":{"accepted":true}})
                        };
                        if node.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                    }
                }
            }
        });
        Ok(Self {
            frames,
            appends,
            acks,
            node_token,
            task: Some(task),
        })
    }

    async fn next_frame(&mut self) -> Result<(String, Value)> {
        tokio::time::timeout(Duration::from_secs(10), self.frames.recv())
            .await?
            .context("frame")
    }

    /// The next frame that is not a `lineage.fence`.
    async fn next_work_frame(&mut self) -> Result<(String, Value)> {
        loop {
            let (method, params) = self.next_frame().await?;
            if method != "lineage.fence" {
                return Ok((method, params));
            }
        }
    }

    /// Everything already received.
    fn drain(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(frame) = self.frames.try_recv() {
            out.push(frame);
        }
        out
    }

    fn disconnect(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    bootstrap: String,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        Self::boot_with(false).await
    }

    /// `supervisor`: run the C1 supervisor on the tick and at boot.
    async fn boot_with(supervisor: bool) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let mut config = HubConfig::for_test(data.clone());
        config.restart_supervisor = supervisor;
        let bootstrap = config.bootstrap_token.clone();
        let hub = spawn(config).await?;
        let human = hub.mint_device_token("restart-phone").await?;
        Ok(Self {
            _dir: dir,
            data,
            bootstrap,
            hub,
            http: reqwest::Client::new(),
            human,
            host: HostId::new().as_id().as_str().to_owned(),
        })
    }

    /// Restart the Hub process on the same data directory.
    async fn restart_hub(&mut self, supervisor: bool) -> Result<()> {
        self.hub.shutdown_within(Duration::from_secs(10)).await;
        let mut config = HubConfig::for_test(self.data.clone());
        config.bootstrap_token = self.bootstrap.clone();
        config.restart_supervisor = supervisor;
        self.hub = spawn(config).await?;
        Ok(())
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        Ok(rusqlite::Connection::open(self.data.join("hub.sqlite"))?)
    }

    /// A claude-sdk continuity seat with `restart`; returns its id after the
    /// Node received its create.
    async fn seat_with(&self, node: &mut FakeNode, restart: Value) -> Result<String> {
        let created: Value = self
            .http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(&self.human)
            .json(&json!({
                "hostId": self.host, "kind": "claude", "driver": "claude-sdk",
                "name": "main", "title": "Main", "grants": ["address-owner"],
                "permissionMode": "auto", "model": "claude-test-model",
                "restart": restart,
                "prompt": "seat brief",
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("seat")?
            .to_owned();
        let (method, _) = node.next_work_frame().await?;
        assert_eq!(method, "instance.create");
        Ok(id)
    }

    async fn seat(&self, node: &mut FakeNode) -> Result<String> {
        self.seat_with(node, json!({"onProcessLoss": true, "maxPerHour": 3}))
            .await
    }

    async fn instance(&self, id: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}/v1/instances/{id}", self.base()))
            .bearer_auth(&self.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn lineage(&self, id: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}/v1/lineages/{id}", self.base()))
            .bearer_auth(&self.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn wait_until<F, Fut, T>(&self, probe: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(value) = probe().await {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("condition never held")
    }

    async fn wait_instance(&self, id: &str, check: impl Fn(&Value) -> bool) -> Result<Value> {
        self.wait_until(|| async {
            let view = self.instance(id).await.ok()?;
            check(&view).then_some(view)
        })
        .await
    }

    /// The chapter reports its native session, then running.
    async fn report_running(&self, node: &FakeNode, id: &str, session: &str) -> Result<()> {
        node.appends.send((
            id.to_owned(),
            json!({"kind":"lifecycle","payload":{"type":"native","topic":"session",
                "nativeName":"session","nativeId":{"state":"known","value":session},
                "status":{"state":"known","value":"started"},"relatedIds":{},"dataRef":null,
                "severity":"info","affectsCompletion":false}}),
        ))?;
        node.appends.send((
            id.to_owned(),
            json!({"kind":"lifecycle","payload":{"type":"entity","state":"ready",
                "reasonCode":"driver-started"}}),
        ))?;
        self.wait_instance(id, |view| {
            view["nativeSessionId"] == json!(session) && view["lifecycle"] == json!("running")
        })
        .await?;
        Ok(())
    }

    /// The claude-sdk process ends (killed): Node-attested process-end
    /// evidence (`topic=session`, `status=failed`).
    async fn report_killed(&self, node: &FakeNode, id: &str) -> Result<()> {
        node.appends.send((
            id.to_owned(),
            json!({"kind":"lifecycle","observedAt":"2026-11-01T01:00:00.000Z","payload":{
                "type":"native","topic":"session","nativeName":"session",
                "status":{"state":"known","value":"failed"},"severity":"error",
                "affectsCompletion":true,"relatedIds":{"exitCode":"137","signal":"KILL"}}}),
        ))?;
        self.wait_instance(id, |view| {
            matches!(view["lifecycle"].as_str(), Some("exited" | "failed"))
        })
        .await?;
        Ok(())
    }

    /// The launch never started (the Node attests a start failure).
    async fn report_start_failed(&self, node: &FakeNode, id: &str) -> Result<()> {
        node.appends.send((
            id.to_owned(),
            json!({"kind":"lifecycle","observedAt":"2026-11-01T01:00:00.000Z","payload":{
                "type":"native","topic":"session","nativeName":"native-driver-start-failed",
                "status":{"state":"known","value":"failed"},"severity":"error",
                "affectsCompletion":true,"relatedIds":{
                    "reasonCode":"native-driver-start-failed",
                    "lastError":"agent process exited during startup"}}}),
        ))?;
        self.wait_instance(id, |view| view["lifecycle"] == json!("failed"))
            .await?;
        Ok(())
    }

    /// `(cause, outcome, successor)` rows of the lineage's decision log.
    fn decisions(&self, lineage: &str) -> Result<Vec<DecisionRow>> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT failed_instance_id, cause, outcome, successor_instance_id
               FROM lineage_restart_decisions WHERE lineage_id = ?1
              ORDER BY decided_at, failed_generation",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![lineage], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn resume(&self, id: &str) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}/v1/instances/{id}/resume", self.base()))
            .bearer_auth(&self.human)
            .json(&json!({}))
            .send()
            .await?)
    }

    async fn close(&self, id: &str) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}/v1/instances/{id}/commands", self.base()))
            .bearer_auth(&self.human)
            .json(&json!({"operation": "instance.close", "payload": {}}))
            .send()
            .await?)
    }

    async fn supervise(&self) -> Vec<Value> {
        self.hub.test_handle().test_supervise().await
    }
}

/// Serializes the suites that install the process-wide continuation hook.
static RACE_HOOK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::const_new(()));

/// A parked hook point: `reached` counts arrivals; every arrival stays parked
/// until `release`. Released gates stay open.
#[derive(Default)]
struct Gate {
    state: Mutex<(usize, bool)>,
    cv: std::sync::Condvar,
}

impl Gate {
    fn arrive(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        self.cv.notify_all();
        while !state.1 {
            state = self.cv.wait(state).unwrap();
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.1 = true;
        self.cv.notify_all();
    }

    async fn reached(&self, arrivals: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.state.lock().unwrap().0 < arrivals {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("gate never reached")
    }
}

/// Install a hook that parks `point` for `lineage` on `gate`.
fn park(point: &'static str, lineage: &str, gate: &Arc<Gate>) {
    let (gate, lineage) = (Arc::clone(gate), lineage.to_owned());
    remuda_hub::lineage_test_support::set_hook(Some(Arc::new(move |at, id| {
        if at == point && id == lineage {
            gate.arrive();
        }
    })));
}

// ── §6.3 row 1: the process ended by itself after reporting running ─────

/// One decision: F with successor (the predecessor fenced, generation 2,
/// chapterCause `process-exit`), recovery from the failed chapter's own
/// native session as a claude-sdk `instance.resume` on the same host, the
/// fence delivered before the create, and the create carrying the lineage's
/// first launch origin (Human, OA5) with the supervisor as actor.
#[tokio::test]
async fn a_process_exit_after_running_restarts_once_as_a_native_claude_sdk_resume() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    assert_eq!(ctx.lineage(&seat).await?["state"], json!("running"));
    ctx.report_killed(&node, &seat).await?;

    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["cause"], json!("process-exit"));
    assert_eq!(decided[0]["outcome"], json!("restarted"));
    let successor = decided[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();

    let (first, fence) = node.next_frame().await?;
    assert_eq!(first, "lineage.fence", "the fence precedes the create");
    assert_eq!(fence["liveGeneration"], json!("2"));
    let (method, create) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["instanceId"], json!(successor));
    assert_eq!(create["origin"], json!("human"), "OA5: the seating origin");
    assert_eq!(create["spec"]["driver"], json!("claude-sdk"));
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    assert_eq!(create["spec"]["hostId"], json!(ctx.host));
    assert_eq!(create["lineage"]["generation"], json!("2"));

    let view = ctx.instance(&successor).await?;
    assert_eq!(view["chapterCause"], json!("process-exit"));
    assert_eq!(view["generation"], json!(2));
    assert_eq!(view["lineageId"], json!(seat));
    assert_eq!(
        view["restart"],
        json!({"onProcessLoss": true, "maxPerHour": 3})
    );
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["generation"], json!(2));
    assert_eq!(record["state"], json!("starting"));
    assert_eq!(record["restartDecisions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        record["restartDecisions"][0]["failedInstanceId"],
        json!(seat)
    );
    assert_eq!(
        record["restartDecisions"][0]["recovery"],
        json!("chapter-session")
    );
    assert_eq!(record["fences"][0]["reason"], json!("superseded"));
    assert_eq!(record["fences"][0]["actor"]["by"], json!("hub-supervisor"));
    assert!(
        record["fences"][0]["actor"]["seatedBy"].is_string(),
        "the seating device stands beside the supervisor: {record}"
    );
    assert!(record["chapters"][0]["fencedAt"].is_string());

    // A second evaluation decides nothing: the key exists and the failed
    // chapter is no longer current.
    assert!(ctx.supervise().await.is_empty());
    assert_eq!(ctx.decisions(&seat)?.len(), 1);
    // Resume and DELETE of the restarting lineage: 409 (§6.3 last rows).
    let resume = ctx.resume(&successor).await?;
    assert_eq!(resume.status(), 409);
    let body: Value = resume.json().await?;
    assert_eq!(body["reason"], json!("not-paused"), "{body}");
    let delete = ctx
        .http
        .delete(format!("{}/v1/instances/{successor}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(delete.status(), 409);
    Ok(())
}

// ── §6.3 row 2: start failure ────────────────────────────────────────────

/// A chapter that never started is a start failure (`start-failed`), and
/// with no session anywhere the successor is a fresh launch of the original
/// create spec (recovery source 3), still claude-sdk on the same host.
#[tokio::test]
async fn a_start_failure_restarts_fresh_from_the_original_spec_with_cause_start_failed()
-> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_start_failed(&node, &seat).await?;
    assert_eq!(
        ctx.lineage(&seat).await?["restartDecisions"],
        json!([]),
        "nothing decides before the supervisor runs"
    );

    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["cause"], json!("start-failed"));
    let (method, create) = node.next_work_frame().await?;
    assert_eq!(
        method, "instance.create",
        "no session anywhere: a fresh launch"
    );
    assert!(create["spec"].get("resumeSessionId").is_none());
    assert_eq!(create["spec"]["driver"], json!("claude-sdk"));
    assert_eq!(create["spec"]["model"], json!("claude-test-model"));
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["restartDecisions"][0]["recovery"], json!("fresh"));
    assert_eq!(record["chapters"][1]["chapterCause"], json!("start-failed"));
    Ok(())
}

/// A process that ends before its first running event (no session started,
/// no ready entity) is a start failure too, whatever its exit shape.
#[tokio::test]
async fn an_end_before_the_first_running_event_is_a_start_failure() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    assert_eq!(ctx.lineage(&seat).await?["state"], json!("starting"));
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    assert_eq!(decided[0]["cause"], json!("start-failed"), "{decided:?}");
    let (method, _) = node.next_work_frame().await?;
    assert_eq!(method, "instance.create");
    Ok(())
}

// ── §6.3 row 3: node-epoch-changed without the chapter ─────────────────

#[tokio::test]
async fn a_node_restart_without_the_chapter_restarts_with_cause_node_restarted() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host,
        &ctx.hub.mint_enroll_token(5).await?,
        true,
        json!({"nodeEpoch": "epoch-a", "instances": [], "instanceStoreFound": true}),
    )
    .await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let token = node.node_token.clone().context("nodeToken")?;
    node.disconnect();
    // The Node restarts: a new epoch whose inventory does not hold the seat.
    let mut node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host,
        &token,
        true,
        json!({"nodeEpoch": "epoch-b", "instances": [], "instanceStoreFound": true}),
    )
    .await?;
    ctx.wait_instance(&seat, |view| view["lifecycle"] == json!("exited"))
        .await?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["cause"], json!("node-restarted"));
    let (method, create) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    Ok(())
}

// ── §6.3 row 6: host link lost — nothing ────────────────────────────────

/// Host loss is contact loss: the process may be alive (D-019). Neither the
/// lost link nor the host-lost sweep is evidence, and a same-epoch reconnect
/// that still holds the chapter keeps it running. No decision anywhere.
#[tokio::test]
async fn a_lost_host_link_is_no_action_and_a_same_epoch_reconnect_keeps_the_chapter() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.host_lost_grace_ms = 1;
    let hub = spawn(config).await?;
    let ctx = Ctx {
        data: dir.path().join("data"),
        _dir: dir,
        bootstrap: String::new(),
        human: hub.mint_device_token("restart-phone").await?,
        hub,
        http: reqwest::Client::new(),
        host: HostId::new().as_id().as_str().to_owned(),
    };
    let mut node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host,
        &ctx.hub.mint_enroll_token(5).await?,
        true,
        json!({"nodeEpoch": "epoch-a", "instances": [], "instanceStoreFound": true}),
    )
    .await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let token = node.node_token.clone().context("nodeToken")?;
    node.disconnect();
    // The sweep marks contact loss (exited, host-contact-lost, no ended_at).
    ctx.wait_instance(&seat, |view| {
        view["lastError"] == json!("host-contact-lost")
    })
    .await?;
    assert_eq!(ctx.lineage(&seat).await?["state"], json!("host-offline"));
    assert!(
        ctx.supervise().await.is_empty(),
        "host loss is not evidence"
    );
    // The same Node process reconnects holding the chapter: running again.
    let _node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host,
        &token,
        true,
        json!({"nodeEpoch": "epoch-a", "instanceStoreFound": true,
               "instances": [{"id": seat, "lifecycle": "running", "activity": "idle"}]}),
    )
    .await?;
    ctx.wait_instance(&seat, |view| view["lifecycle"] == json!("running"))
        .await?;
    assert!(ctx.supervise().await.is_empty());
    assert!(ctx.decisions(&seat)?.is_empty());
    assert_eq!(ctx.lineage(&seat).await?["generation"], json!(1));
    Ok(())
}

// ── §6.3 row 5: no restart.onProcessLoss → paused{process-exit} ─────────

#[tokio::test]
async fn a_lineage_without_on_process_loss_pauses_with_process_exit() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx
        .seat_with(&mut node, json!({"onProcessLoss": false, "maxPerHour": 3}))
        .await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["outcome"], json!("process-exit"));
    assert!(decided[0].get("successorInstanceId").is_none());
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("paused"));
    assert_eq!(record["pausedBy"]["kind"], json!("process-exit"));
    assert_eq!(record["generation"], json!(2));
    assert_eq!(record["chapters"].as_array().map(Vec::len), Some(1));
    let (first, _) = node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    // Owner Resume continues a paused lineage.
    let resumed: Value = ctx.resume(&seat).await?.error_for_status()?.json().await?;
    assert_eq!(resumed["instance"]["chapterCause"], json!("owner-resume"));
    Ok(())
}

// ── §6.3 row 4: the cap ─────────────────────────────────────────────────

/// The cap counts restart decisions of every cause in the last hour; at the
/// cap the supervisor runs F without successor (`paused{restart-cap}`),
/// which fences the failed chapter; an owner Resume then continues.
#[tokio::test]
async fn the_cap_counts_every_cause_and_pauses_with_restart_cap() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx
        .seat_with(&mut node, json!({"onProcessLoss": true, "maxPerHour": 2}))
        .await?;
    // Restart 1: a start failure.
    ctx.report_start_failed(&node, &seat).await?;
    let first = ctx.supervise().await;
    assert_eq!(first[0]["cause"], json!("start-failed"));
    let second_chapter = first[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let _ = node.next_work_frame().await?;
    // Restart 2: a process exit.
    ctx.report_running(&node, &second_chapter, SESSION).await?;
    ctx.report_killed(&node, &second_chapter).await?;
    let second = ctx.supervise().await;
    assert_eq!(second[0]["cause"], json!("process-exit"));
    let third_chapter = second[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let _ = node.next_work_frame().await?;
    // The third failure is at the cap: paused{restart-cap}, no successor.
    ctx.report_running(&node, &third_chapter, SESSION).await?;
    ctx.report_killed(&node, &third_chapter).await?;
    let capped = ctx.supervise().await;
    assert_eq!(capped.len(), 1, "{capped:?}");
    assert_eq!(capped[0]["outcome"], json!("restart-cap"));
    assert!(capped[0].get("successorInstanceId").is_none());
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("paused"), "{record}");
    assert_eq!(record["pausedBy"]["kind"], json!("restart-cap"));
    assert_eq!(record["chapters"].as_array().map(Vec::len), Some(3));
    assert!(
        record["chapters"][2]["fencedAt"].is_string(),
        "the cap fences"
    );
    assert!(ctx.supervise().await.is_empty());
    let outcomes: Vec<String> = ctx
        .decisions(&seat)?
        .into_iter()
        .map(|(_, _, outcome, _)| outcome)
        .collect();
    assert_eq!(outcomes, ["restarted", "restarted", "restart-cap"]);
    // Owner Resume continues; it is not a restart decision.
    let resumed: Value = ctx
        .resume(&third_chapter)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(resumed["instance"]["chapterCause"], json!("owner-resume"));
    assert_eq!(resumed["instance"]["generation"], json!(4));
    Ok(())
}

// ── §6.3.1 recovery-source order ────────────────────────────────────────

/// 1. the failed chapter's own session; 2. else the newest earlier chapter
/// that reported one; 3. else a fresh launch (covered by the start-failure
/// test above).
#[tokio::test]
async fn the_recovery_source_is_the_chapter_then_the_newest_earlier_session() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, EARLIER_SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let first = ctx.supervise().await;
    let second = first[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let (_, create) = node.next_work_frame().await?;
    assert_eq!(create["spec"]["resumeSessionId"], json!(EARLIER_SESSION));
    // Chapter 2 reports its own session and dies: its own session wins.
    ctx.report_running(&node, &second, SESSION).await?;
    ctx.report_killed(&node, &second).await?;
    let next = ctx.supervise().await;
    let third = next[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let (_, create) = node.next_work_frame().await?;
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    // Chapter 3 fails to start without a session: the newest earlier one.
    ctx.report_start_failed(&node, &third).await?;
    let last = ctx.supervise().await;
    assert_eq!(last[0]["cause"], json!("start-failed"));
    let (method, create) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    let record = ctx.lineage(&seat).await?;
    let recoveries: Vec<Value> = record["restartDecisions"]
        .as_array()
        .context("decisions")?
        .iter()
        .map(|row| row["recovery"].clone())
        .collect();
    assert_eq!(
        recoveries,
        [
            json!("chapter-session"),
            json!("chapter-session"),
            json!("earlier-session")
        ]
    );
    Ok(())
}

// ── Exactly one decision per failed chapter ─────────────────────────────

/// Two evaluations read the same pending failure, then race: one F commits,
/// the other finds the key (or a moved generation) and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn double_evaluation_decides_a_failed_chapter_once() -> Result<()> {
    let _serial = RACE_HOOK.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let gate = Arc::new(Gate::default());
    park("restart-read", &seat, &gate);
    let (a, b) = (ctx.hub.test_handle(), ctx.hub.test_handle());
    let first = tokio::spawn(async move { a.test_supervise().await });
    let second = tokio::spawn(async move { b.test_supervise().await });
    gate.reached(2).await?;
    gate.release();
    let (first, second) = (first.await?, second.await?);
    remuda_hub::lineage_test_support::set_hook(None);
    assert_eq!(first.len() + second.len(), 1, "{first:?} {second:?}");
    assert_eq!(ctx.decisions(&seat)?.len(), 1);
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["chapters"].as_array().map(Vec::len), Some(2));
    assert_eq!(record["generation"], json!(2));
    let creates = node
        .drain()
        .into_iter()
        .filter(|(method, _)| method == "instance.resume")
        .count();
    assert!(creates <= 1, "at most one successor create");
    Ok(())
}

/// A Hub restart with a failure pending: the boot evaluation decides it
/// once, and a second restart decides nothing again.
#[tokio::test]
async fn a_hub_restart_decides_a_pending_failure_exactly_once() -> Result<()> {
    let mut ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let token = node.node_token.clone().context("nodeToken")?;
    drop(node);
    // The Hub goes down with the failure undecided and comes back with the
    // supervisor on: the boot evaluation decides it before any Node returns.
    ctx.restart_hub(true).await?;
    assert_eq!(ctx.decisions(&seat)?.len(), 1, "decided at boot");
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["generation"], json!(2));
    // The Node returns: the fence first, then the successor's create, once.
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, true, json!({})).await?;
    let (first, _) = node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    let (method, create) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    let token = node.node_token.clone().unwrap_or(token);
    drop(node);
    ctx.restart_hub(true).await?;
    assert_eq!(
        ctx.decisions(&seat)?.len(),
        1,
        "a second boot decides nothing"
    );
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, true, json!({})).await?;
    // Nothing is re-sent for the decided failure: the create was forwarded,
    // so the next frame on the link is an unrelated plain create.
    let probe: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(
            &json!({"hostId": ctx.host, "kind": "claude", "driver": "claude-print",
                      "permissionMode": "manual", "prompt": "probe"}),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let (method, params) = node.next_work_frame().await?;
    assert_eq!(method, "instance.create");
    assert_eq!(params["instanceId"], probe["instance"]["instanceId"]);
    assert_eq!(
        ctx.lineage(&seat).await?["chapters"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    Ok(())
}

/// The supervisor runs on the scheduler tick by itself.
#[tokio::test]
async fn the_scheduler_tick_decides_without_a_test_hook() -> Result<()> {
    let ctx = Ctx::boot_with(true).await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let (method, _) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume", "the tick restarted the seat");
    assert_eq!(ctx.decisions(&seat)?.len(), 1);
    Ok(())
}

// ── A close always wins ─────────────────────────────────────────────────

/// A close fences at admission; an exit that follows finds the lineage
/// paused{device} and is ignored. The Hub never undoes the owner's stop.
#[tokio::test]
async fn an_exit_after_a_close_is_ignored() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    assert_eq!(ctx.close(&seat).await?.status(), 200);
    ctx.report_killed(&node, &seat).await?;
    assert!(ctx.supervise().await.is_empty());
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["state"], json!("paused"));
    assert_eq!(record["pausedBy"]["kind"], json!("device"));
    assert!(ctx.decisions(&seat)?.is_empty());
    Ok(())
}

/// The supervisor read the evidence first; the close commits while it is
/// parked; its F then loses the CAS and writes no decision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_committed_while_the_supervisor_decides_wins() -> Result<()> {
    let _serial = RACE_HOOK.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let gate = Arc::new(Gate::default());
    park("restart-read", &seat, &gate);
    let handle = ctx.hub.test_handle();
    let supervisor = tokio::spawn(async move { handle.test_supervise().await });
    gate.reached(1).await?;
    // The owner's close of the ended chapter pauses the lineage.
    let closed = ctx.close(&seat).await?;
    assert_eq!(closed.status(), 200);
    gate.release();
    let decided = supervisor.await?;
    remuda_hub::lineage_test_support::set_hook(None);
    assert!(decided.is_empty(), "{decided:?}");
    assert!(ctx.decisions(&seat)?.is_empty());
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["pausedBy"]["kind"], json!("device"));
    assert_eq!(record["chapters"].as_array().map(Vec::len), Some(1));
    Ok(())
}

/// A racing owner Resume (allowed on a chapter with process-end evidence)
/// commits first; the supervisor then finds the failed chapter superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_racing_resume_leaves_the_supervisor_nothing_to_decide() -> Result<()> {
    let _serial = RACE_HOOK.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let gate = Arc::new(Gate::default());
    park("restart-read", &seat, &gate);
    let handle = ctx.hub.test_handle();
    let supervisor = tokio::spawn(async move { handle.test_supervise().await });
    gate.reached(1).await?;
    let resumed: Value = ctx.resume(&seat).await?.error_for_status()?.json().await?;
    assert_eq!(resumed["instance"]["chapterCause"], json!("owner-resume"));
    gate.release();
    let decided = supervisor.await?;
    remuda_hub::lineage_test_support::set_hook(None);
    assert!(decided.is_empty(), "{decided:?}");
    assert!(ctx.decisions(&seat)?.is_empty());
    assert_eq!(
        ctx.lineage(&seat).await?["chapters"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    Ok(())
}
