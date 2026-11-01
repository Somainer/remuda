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
use rusqlite::OptionalExtension;
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
    acks: AckScript,
    /// Methods answered with a JSON-RPC error `(code, message)`.
    errors: Arc<Mutex<std::collections::HashMap<String, (i64, String)>>>,
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
        // A hello without `capabilities` keeps the previous blob; a Node
        // without fence support sends capabilities that lack it.
        params["capabilities"] = if fence_capable {
            json!({ "lineageFence": true })
        } else {
            json!({})
        };
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
        let errors: Arc<Mutex<std::collections::HashMap<String, (i64, String)>>> = Arc::default();
        let task_errors = Arc::clone(&errors);
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
                        let scripted = task_errors.lock().unwrap().get(&method).cloned();
                        let reply = if let Some((code, message)) = scripted {
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
                        } else if method == "lineage.fence" {
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
                            // D-047: a launch echoes the route it was asked for.
                            let mut result = json!({"accepted": true});
                            if matches!(method.as_str(), "instance.create" | "instance.resume")
                                && let Some(route) = params.pointer("/spec/apiRoute").cloned()
                            {
                                result["apiRoute"] = route;
                            }
                            json!({"jsonrpc":"2.0","id":id,"result":result})
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
            errors,
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

    /// The next `instance.create` / `instance.resume` (fences and restart
    /// notices skipped).
    async fn next_launch(&mut self) -> Result<(String, Value)> {
        loop {
            let (method, params) = self.next_frame().await?;
            if matches!(method.as_str(), "instance.create" | "instance.resume") {
                return Ok((method, params));
            }
        }
    }

    /// The next `instance.send` (fences and launches skipped).
    async fn next_send(&mut self) -> Result<Value> {
        loop {
            let (method, params) = self.next_frame().await?;
            if method == "instance.send" {
                return Ok(params);
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
        self.restart_hub_editing(supervisor, |_| Ok(())).await
    }

    /// Restart the Hub, applying `edit` to its database while it is down.
    async fn restart_hub_editing(
        &mut self,
        supervisor: bool,
        edit: impl FnOnce(&rusqlite::Connection) -> Result<()>,
    ) -> Result<()> {
        self.hub.shutdown_within(Duration::from_secs(10)).await;
        edit(&self.db()?)?;
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
        let (method, _) = node.next_launch().await?;
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
    let (method, create) = node.next_launch().await?;
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
    let (method, create) = node.next_launch().await?;
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
    let (method, _) = node.next_launch().await?;
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
    let (method, create) = node.next_launch().await?;
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
    assert_eq!(ctx.end_seen_at(&seat)?, None, "contact loss is not an end");
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
    let _ = node.next_launch().await?;
    // Restart 2: a process exit.
    ctx.report_running(&node, &second_chapter, SESSION).await?;
    ctx.report_killed(&node, &second_chapter).await?;
    let second = ctx.supervise().await;
    assert_eq!(second[0]["cause"], json!("process-exit"));
    let third_chapter = second[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let _ = node.next_launch().await?;
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
    let (_, create) = node.next_launch().await?;
    assert_eq!(create["spec"]["resumeSessionId"], json!(EARLIER_SESSION));
    // Chapter 2 reports its own session and dies: its own session wins.
    ctx.report_running(&node, &second, SESSION).await?;
    ctx.report_killed(&node, &second).await?;
    let next = ctx.supervise().await;
    let third = next[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let (_, create) = node.next_launch().await?;
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    // Chapter 3 fails to start without a session: the newest earlier one.
    ctx.report_start_failed(&node, &third).await?;
    let last = ctx.supervise().await;
    assert_eq!(last[0]["cause"], json!("start-failed"));
    let (method, create) = node.next_launch().await?;
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
    // The Node returns: the fence first, then the successor's create, then
    // (the tick) the restart notice whose id F chose before the crash.
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, true, json!({})).await?;
    let (first, _) = node.next_frame().await?;
    assert_eq!(first, "lineage.fence");
    let (method, create) = node.next_work_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["spec"]["resumeSessionId"], json!(SESSION));
    let notice = node.next_send().await?;
    assert_eq!(
        notice["commandId"],
        ctx.lineage(&seat).await?["restartDecisions"][0]["noticeCommandId"]
    );
    assert_eq!(notice["origin"], json!("hub"));
    let token = node.node_token.clone().unwrap_or(token);
    drop(node);
    ctx.restart_hub(true).await?;
    assert_eq!(
        ctx.decisions(&seat)?.len(),
        1,
        "a second boot decides nothing"
    );
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, true, json!({})).await?;
    // Nothing is re-sent for the decided failure — neither the create nor
    // the notice — so the next frame on the link is an unrelated create.
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
    let (method, _) = node.next_launch().await?;
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

// ── §6.4 the restart notice ─────────────────────────────────────────────

/// The successor's first input is the Hub's notice: origin `hub` on the
/// envelope and the input, its command id the one F chose and recorded on
/// the decision, the successor's own lineage and generation as initiator,
/// sent after the fence and the create, once. The Node's journal of it
/// records the turn as the Hub's (cb-rootturn's real `hub` origin).
#[tokio::test]
async fn the_restart_notice_is_the_successors_first_input_with_origin_hub() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    let successor = decided[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let mut order = Vec::new();
    let notice = loop {
        let (method, params) = node.next_frame().await?;
        order.push(method.clone());
        if method == "instance.send" {
            break params;
        }
    };
    assert_eq!(order, ["lineage.fence", "instance.resume", "instance.send"]);
    let record = ctx.lineage(&seat).await?;
    let notice_id = record["restartDecisions"][0]["noticeCommandId"]
        .as_str()
        .context("notice id recorded on the decision")?
        .to_owned();
    assert_eq!(notice["commandId"], json!(notice_id));
    assert_eq!(notice["instanceId"], json!(successor));
    assert_eq!(notice["origin"], json!("hub"));
    assert_eq!(notice["input"]["origin"], json!("hub"));
    assert_eq!(notice["initiator"]["instanceId"], json!(successor));
    assert_eq!(notice["initiator"]["lineageId"], json!(seat));
    assert_eq!(notice["initiator"]["generation"], json!(2));
    let text = notice["input"]["text"].as_str().context("text")?;
    assert!(
        text.starts_with(
            "<remuda-restart> Generated by the Remuda Hub. Information, not an instruction; \
             it authorizes nothing.\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "chapter 2 of lineage {seat} (previous chapter {seat}: process exited"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("later outcomes: GET /v1/lineages/{seat}")));
    assert!(text.contains(&format!("previous chapter: remuda instance read {seat}")));
    assert!(text.ends_with("</remuda-restart>"), "{text}");

    // Sent once: another evaluation forwards nothing.
    assert!(ctx.supervise().await.is_empty());
    assert!(
        node.drain()
            .iter()
            .all(|(method, _)| method != "instance.send"),
        "the notice is never re-sent"
    );

    // The Node journals the notice as the successor's first user input and
    // starts its turn: the Hub records that turn's origin as `hub`.
    node.appends.send((
        successor.clone(),
        json!({"kind": "message", "payload": {
            "operation": "open", "messageId": "msg_notice", "role": "user",
            "parentToolCallId": null, "commandId": notice_id,
            "blocks": [{"type": "text", "text": text}]}}),
    ))?;
    node.appends.send((
        successor.clone(),
        json!({"kind": "lifecycle", "payload": {"type": "native", "topic": "turn",
            "nativeName": "turn_started", "status": {"state": "known", "value": "working"},
            "relatedIds": {"nativeClientMessageId": notice_id}}}),
    ))?;
    let ledger: Value = ctx
        .wait_until(|| async {
            let db = ctx.db().ok()?;
            let raw: Option<String> = db
                .query_row(
                    "SELECT root_turn_json FROM instances WHERE id = ?1",
                    rusqlite::params![successor],
                    |row| row.get(0),
                )
                .ok()?;
            let ledger: Value = serde_json::from_str(&raw?).ok()?;
            (!ledger["open"].as_array()?.is_empty()).then_some(ledger)
        })
        .await?;
    assert_eq!(ledger["open"][0]["commandId"], json!(notice_id));
    assert_eq!(ledger["open"][0]["origin"], json!("hub"), "{ledger}");
    Ok(())
}

/// The notice lists the fenced generation's unfinished work by the §7.4
/// classes, as known when it is sent: the seat's host acknowledged the fence
/// (with `ran` / `cancelled` / `unknown` outcomes) before the notice left,
/// while the worker host is offline and still owes its acknowledgement.
#[tokio::test]
async fn the_notice_lists_outcomes_by_class_as_known_when_it_is_sent() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let worker_host = HostId::new().as_id().as_str().to_owned();
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let mut worker_node = FakeNode::connect(&ctx.hub, &worker_host, true).await?;
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
            "name": "main", "title": "Main", "grants": ["address-owner", "dispatch"],
            "permissionMode": "auto", "model": "claude-test-model",
            "scope": {"hostIds": [ctx.host, worker_host]},
            "restart": {"onProcessLoss": true, "maxPerHour": 3},
            "prompt": "seat brief",
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let seat = created["instance"]["instanceId"]
        .as_str()
        .context("seat")?
        .to_owned();
    let (_, params) = node.next_launch().await?;
    let token = params["agentCredential"]["token"]
        .as_str()
        .context("token")?
        .to_owned();
    // Owner-created workers the seat may message: one on each host.
    let worker = |host: String| {
        let ctx = &ctx;
        async move {
            let created: Value = ctx
                .http
                .post(format!("{}/v1/instances", ctx.base()))
                .bearer_auth(&ctx.human)
                .json(
                    &json!({"hostId": host, "kind": "claude", "driver": "claude-print",
                              "permissionMode": "manual", "prompt": "worker brief"}),
                )
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            anyhow::Ok(
                created["instance"]["instanceId"]
                    .as_str()
                    .context("worker")?
                    .to_owned(),
            )
        }
    };
    let local = worker(ctx.host.clone()).await?;
    let _ = node.next_launch().await?;
    let remote = worker(worker_host.clone()).await?;
    let _ = worker_node.next_launch().await?;
    let send = |target: String, text: &'static str| {
        let ctx = &ctx;
        let token = token.clone();
        async move {
            let command_id = remuda_protocol::CommandId::new()
                .as_id()
                .as_str()
                .to_owned();
            let response = ctx
                .http
                .post(format!("{}/v1/instances/{target}/commands", ctx.base()))
                .bearer_auth(&token)
                .json(
                    &json!({"operation": "instance.send", "commandId": command_id,
                              "payload": {"input": {"type": "prompt", "text": text}}}),
                )
                .send()
                .await?;
            assert!(response.status().is_success(), "{}", response.text().await?);
            anyhow::Ok(command_id)
        }
    };
    let ran = send(local.clone(), "ran on the seat host").await?;
    let cancelled = send(local.clone(), "cancelled by the seat host").await?;
    let unknown = send(local.clone(), "unknown after a crash").await?;
    let in_flight = send(remote.clone(), "in flight to the offline host").await?;
    worker_node.disconnect();
    ctx.wait_until(|| async {
        let hosts: Value = ctx
            .http
            .get(format!("{}/v1/hosts/{worker_host}", ctx.base()))
            .bearer_auth(&ctx.human)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (hosts["online"] == json!(false)).then_some(())
    })
    .await?;
    let held = send(remote.clone(), "held for the offline host").await?;
    node.acks.lock().unwrap().push_back(vec![
        (ran.clone(), "ran".into()),
        (cancelled.clone(), "cancelled".into()),
        (unknown.clone(), "unknown".into()),
    ]);
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    let notice = loop {
        let send = node.next_send().await?;
        if send["origin"] == json!("hub") {
            break send;
        }
    };
    let text = notice["input"]["text"].as_str().context("text")?;
    for expected in [
        format!(
            "cancelled, never left the Hub: {held} instance.send -> instance {remote} \
             \"held for the offline host\""
        ),
        format!("ran before its host applied the fence: {ran} instance.send -> instance {local}"),
        format!(
            "cancelled by host {} when it applied the fence: {cancelled} instance.send -> \
             instance {local}",
            ctx.host
        ),
        format!(
            "unknown, host {} applied the fence but cannot tell after a crash: {unknown} \
             instance.send -> instance {local}",
            ctx.host
        ),
        format!("may still run, host {worker_host} has not applied the fence (offline since "),
        format!("): {in_flight} instance.send -> instance {remote}"),
    ] {
        assert!(text.contains(&expected), "missing {expected:?} in:\n{text}");
    }
    Ok(())
}

/// §6.4 / §7.6: hub-origin input goes only to Nodes that advertise the fence
/// capability. The successor's host returns without it: the notice is
/// settled `rejected(node-lacks-fence)` and no `instance.send` reaches it.
#[tokio::test]
async fn a_notice_is_never_sent_to_a_node_without_fence_support() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let token = node.node_token.clone().context("nodeToken")?;
    node.disconnect();
    ctx.wait_until(|| async {
        let host: Value = ctx
            .http
            .get(format!("{}/v1/hosts/{}", ctx.base(), ctx.host))
            .bearer_auth(&ctx.human)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (host["online"] == json!(false)).then_some(())
    })
    .await?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    // The Node comes back downgraded: no lineageFence capability.
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, false, json!({})).await?;
    assert!(ctx.supervise().await.is_empty());
    let notice_id = ctx.lineage(&seat).await?["restartDecisions"][0]["noticeCommandId"]
        .as_str()
        .context("notice")?
        .to_owned();
    let (state, outcome, reason): (String, Option<String>, Option<String>) = ctx.db()?.query_row(
        "SELECT state, settlement_outcome, settlement_reason FROM commands WHERE id = ?1",
        rusqlite::params![notice_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(
        (state.as_str(), outcome.as_deref(), reason.as_deref()),
        ("settled", Some("rejected"), Some("node-lacks-fence"))
    );
    assert!(
        node.drain()
            .iter()
            .all(|(method, _)| method != "instance.send"),
        "no hub-origin input reaches a Node without fence support"
    );
    Ok(())
}

// ── §8.5 C1 pushes ──────────────────────────────────────────────────────

/// Counts Web Push deliveries.
#[derive(Default)]
struct FakePush {
    hits: Mutex<usize>,
    /// While set, every delivery is refused (HTTP 400, not retried).
    fail: std::sync::atomic::AtomicBool,
}

impl remuda_push::Transport for FakePush {
    fn post(
        &self,
        _url: String,
        _headers: Vec<(String, String)>,
        _body: Vec<u8>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u16, remuda_push::Error>> + Send + '_>,
    > {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async { Ok(400) });
        }
        *self.hits.lock().unwrap() += 1;
        Box::pin(async { Ok(201) })
    }
}

impl Ctx {
    /// Restart the Hub on the same data dir with push on the same fake.
    async fn restart_hub_push(&mut self, fake: &Arc<FakePush>, supervisor: bool) -> Result<()> {
        self.hub.shutdown_within(Duration::from_secs(10)).await;
        let mut config = HubConfig::for_test(self.data.clone());
        config.bootstrap_token = self.bootstrap.clone();
        config.restart_supervisor = supervisor;
        self.hub = remuda_hub::spawn_with_push(config, fake.clone()).await?;
        Ok(())
    }

    /// `(recipients, sent_at)` of every C1 push row of a lineage, oldest first.
    fn c1_push_rows(&self, lineage: &str) -> Result<Vec<(Option<i64>, Option<String>)>> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT recipients, sent_at FROM lineage_c1_pushes
              WHERE lineage_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![lineage], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A Hub with Web Push on a fake endpoint and one subscribed device.
    async fn boot_push() -> Result<(Self, Arc<FakePush>)> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let config = HubConfig::for_test(data.clone());
        let bootstrap = config.bootstrap_token.clone();
        let fake = Arc::new(FakePush::default());
        let hub = remuda_hub::spawn_with_push(config, fake.clone()).await?;
        let human = hub.mint_device_token("restart-phone").await?;
        let ctx = Self {
            _dir: dir,
            data,
            bootstrap,
            hub,
            http: reqwest::Client::new(),
            human,
            host: HostId::new().as_id().as_str().to_owned(),
        };
        let (pair, auth) = ece::generate_keypair_and_auth_secret().expect("ece");
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let subscribed = ctx
            .http
            .post(format!("{}/push/subscriptions", ctx.base()))
            .bearer_auth(&ctx.human)
            .json(
                &json!({"endpoint": "http://127.0.0.1:9/fake-push", "keys": {
                "p256dh": b64.encode(pair.pub_as_raw().expect("pub")),
                "auth": b64.encode(auth)}}),
            )
            .send()
            .await?;
        anyhow::ensure!(subscribed.status() == 201, "{}", subscribed.text().await?);
        Ok((ctx, fake))
    }

    /// `(kind, title, recipients)` of the lineage's recorded C1 pushes.
    async fn c1_pushes(&self, lineage: &str, count: usize) -> Result<Vec<(String, String, i64)>> {
        self.wait_until(|| async {
            let db = self.db().ok()?;
            let mut stmt = db
                .prepare(
                    "SELECT kind, title, recipients FROM lineage_c1_pushes
                      WHERE lineage_id = ?1 AND recipients IS NOT NULL ORDER BY created_at",
                )
                .ok()?;
            let rows = stmt
                .query_map(rusqlite::params![lineage], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            (rows.len() >= count).then_some(rows)
        })
        .await
    }
}

/// "Main restarted (cause)" replaces the generic exited push on a lineage
/// that restarts on process loss; at the cap, "Main paused: restart cap";
/// a pause from a Human device sends nothing; "Main closed itself" for a
/// self-close. Each is recorded once per F, with its recipient count.
#[tokio::test]
async fn c1_pushes_restart_cap_and_self_close_and_none_for_a_device_pause() -> Result<()> {
    let (ctx, fake) = Ctx::boot_push().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx
        .seat_with(&mut node, json!({"onProcessLoss": true, "maxPerHour": 1}))
        .await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let hits_before = *fake.hits.lock().unwrap();
    // The process ends; the event also carries the generic exited marker.
    node.appends.send((
        seat.clone(),
        json!({"kind":"lifecycle","lifecycle":"exited","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"failed"},"severity":"error",
            "affectsCompletion":true,"relatedIds":{"exitCode":"137"}}}),
    ))?;
    ctx.wait_instance(&seat, |view| {
        matches!(view["lifecycle"].as_str(), Some("exited" | "failed"))
    })
    .await?;
    let decided = ctx.supervise().await;
    let successor = decided[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let pushes = ctx.c1_pushes(&seat, 1).await?;
    assert_eq!(
        pushes,
        [(
            "restarted".to_owned(),
            "Main restarted (process exited)".to_owned(),
            1
        )]
    );
    assert_eq!(
        *fake.hits.lock().unwrap() - hits_before,
        1,
        "the C1 push replaces the generic exited push"
    );

    // The successor dies at the cap (maxPerHour 1).
    let _ = node.next_launch().await?;
    ctx.report_running(&node, &successor, SESSION).await?;
    ctx.report_killed(&node, &successor).await?;
    let capped = ctx.supervise().await;
    assert_eq!(capped[0]["outcome"], json!("restart-cap"));
    let pushes = ctx.c1_pushes(&seat, 2).await?;
    assert_eq!(pushes[1].0, "restart-cap");
    assert_eq!(pushes[1].1, "Main paused: restart cap");

    // Owner Resume, then a Human pause: no push.
    let resumed: Value = ctx
        .resume(&successor)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let third = resumed["instance"]["instanceId"]
        .as_str()
        .context("third")?
        .to_owned();
    let (_, params) = node.next_launch().await?;
    let third_token = params["agentCredential"]["token"]
        .as_str()
        .context("token")?
        .to_owned();
    // The chapter closes itself: paused{self}, "Main closed itself".
    let closed = ctx
        .http
        .post(format!("{}/v1/instances/{third}/commands", ctx.base()))
        .bearer_auth(&third_token)
        .json(&json!({"operation": "instance.close", "payload": {}}))
        .send()
        .await?;
    assert_eq!(closed.status(), 200, "{}", closed.text().await?);
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["pausedBy"]["kind"], json!("self"));
    let pushes = ctx.c1_pushes(&seat, 3).await?;
    assert_eq!(pushes[2].0, "self");
    assert_eq!(pushes[2].1, "Main closed itself");

    // Resume again and pause from the Human device: nothing is pushed.
    let resumed: Value = ctx.resume(&third).await?.error_for_status()?.json().await?;
    let fourth = resumed["instance"]["instanceId"]
        .as_str()
        .context("fourth")?
        .to_owned();
    let _ = node.next_launch().await?;
    let hits_before = *fake.hits.lock().unwrap();
    assert_eq!(ctx.close(&fourth).await?.status(), 200);
    assert_eq!(
        ctx.lineage(&seat).await?["pausedBy"]["kind"],
        json!("device")
    );
    // Drive once more: still three records, nothing new sent.
    assert!(ctx.supervise().await.is_empty());
    assert_eq!(ctx.c1_pushes(&seat, 3).await?.len(), 3);
    assert_eq!(*fake.hits.lock().unwrap(), hits_before);
    Ok(())
}

/// A close by an Agent ancestor pauses the child lineage `paused{ancestor}`
/// and records its C1 push, named by the child's title (it is not the seat).
#[tokio::test]
async fn an_ancestor_close_records_a_c1_push() -> Result<()> {
    let (ctx, fake) = Ctx::boot_push().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
            "name": "main", "title": "Main", "grants": ["address-owner", "dispatch"],
            "permissionMode": "auto", "model": "claude-test-model",
            "scope": {"hostIds": [ctx.host]},
            "restart": {"onProcessLoss": true, "maxPerHour": 3},
            "prompt": "seat brief",
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let seat = created["instance"]["instanceId"]
        .as_str()
        .context("seat")?
        .to_owned();
    let (_, params) = node.next_launch().await?;
    let seat_token = params["agentCredential"]["token"]
        .as_str()
        .context("token")?
        .to_owned();
    // The seat creates a continuity child (it holds a grant) on its host.
    let child = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&seat_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
            "title": "Coordinator", "grants": ["dispatch"],
            "permissionMode": "manual", "model": "claude-test-model",
            "scope": {"hostIds": [ctx.host]}, "prompt": "child brief",
        }))
        .send()
        .await?;
    let status = child.status();
    let child: Value = child.json().await?;
    assert!(status.is_success(), "{child}");
    let child = child["instance"]["instanceId"]
        .as_str()
        .context("child")?
        .to_owned();
    let _ = node.next_launch().await?;
    let hits_before = *fake.hits.lock().unwrap();
    let closed = ctx
        .http
        .post(format!("{}/v1/instances/{child}/commands", ctx.base()))
        .bearer_auth(&seat_token)
        .json(&json!({"operation": "instance.close", "payload": {}}))
        .send()
        .await?;
    assert_eq!(closed.status(), 200, "{}", closed.text().await?);
    let record = ctx.lineage(&child).await?;
    assert_eq!(record["pausedBy"]["kind"], json!("ancestor"));
    assert_eq!(record["pausedBy"]["instanceId"], json!(seat));
    let pushes = ctx.c1_pushes(&child, 1).await?;
    assert_eq!(
        pushes,
        [(
            "ancestor".to_owned(),
            "Coordinator paused by its parent agent".to_owned(),
            1
        )]
    );
    assert_eq!(*fake.hits.lock().unwrap() - hits_before, 1);
    Ok(())
}

/// A lineage without `restart.onProcessLoss` keeps today's exited push and
/// records no C1 push for its `paused{process-exit}`.
#[tokio::test]
async fn a_process_exit_pause_keeps_the_generic_exited_push() -> Result<()> {
    let (ctx, fake) = Ctx::boot_push().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx
        .seat_with(&mut node, json!({"onProcessLoss": false, "maxPerHour": 3}))
        .await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let hits_before = *fake.hits.lock().unwrap();
    node.appends.send((
        seat.clone(),
        json!({"kind":"lifecycle","lifecycle":"exited","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"exited"},"severity":"info",
            "affectsCompletion":true,"relatedIds":{"exitCode":"0"}}}),
    ))?;
    ctx.wait_until(|| async { (*fake.hits.lock().unwrap() > hits_before).then_some(()) })
        .await
        .context("the generic exited push")?;
    let decided = ctx.supervise().await;
    assert_eq!(decided[0]["outcome"], json!("process-exit"));
    let db = ctx.db()?;
    let recorded: i64 = db.query_row(
        "SELECT COUNT(*) FROM lineage_c1_pushes WHERE lineage_id = ?1",
        rusqlite::params![seat],
        |row| row.get(0),
    )?;
    assert_eq!(recorded, 0);
    Ok(())
}

// ── r2 item 1: the successor carries the seating Human grant ────────────

/// OA5: the successor's create carries the seating device's real origin,
/// recorded at create time (`seatedOrigin`), never the spec's incidental
/// `origin` key. The stored spec origin is rewritten to `agent` — what the
/// lineage insert records for a spec without an `origin` key — and the
/// successor is still queued and forwarded as `human`.
#[tokio::test]
async fn the_successor_carries_the_seating_human_origin_not_the_spec_origin() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.db()?.execute(
        "UPDATE lineages SET origin_spec_ref = json_set(origin_spec_ref, '$.origin', 'agent')
          WHERE lineage_id = ?1",
        rusqlite::params![seat],
    )?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    let successor = decided[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let (method, create) = node.next_launch().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(
        create["origin"],
        json!("human"),
        "OA5: the seating Human grant, not the spec's origin"
    );
    let stored: String = ctx.db()?.query_row(
        "SELECT json_extract(payload_json, '$.origin') FROM commands
          WHERE instance_id = ?1 AND operation = 'instance.resume'",
        rusqlite::params![successor],
        |row| row.get(0),
    )?;
    assert_eq!(stored, "human");
    let (seated_origin, seated_by): (Option<String>, Option<String>) = ctx.db()?.query_row(
        "SELECT json_extract(origin_spec_ref, '$.seatedOrigin'),
                json_extract(origin_spec_ref, '$.deviceId')
           FROM lineages WHERE lineage_id = ?1",
        rusqlite::params![seat],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(seated_origin.as_deref(), Some("human"));
    assert!(seated_by.is_some());
    Ok(())
}

// ── r2 item 2: a C1 push is sent before it is marked sent ───────────────

/// A failed fanout leaves the push unsent (no `sent_at`, `recipients` NULL);
/// the next evaluation retries it, and so does the boot evaluation after a
/// Hub restart.
#[tokio::test]
async fn a_failed_c1_push_is_retried_next_tick_and_after_a_hub_restart() -> Result<()> {
    use std::sync::atomic::Ordering;
    let (mut ctx, fake) = Ctx::boot_push().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    fake.fail.store(true, Ordering::SeqCst);
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    let successor = decided[0]["successorInstanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    assert_eq!(
        ctx.c1_push_rows(&seat)?,
        [(None, None)],
        "a failed fanout is not marked sent"
    );
    // The next tick retries it once delivery works.
    fake.fail.store(false, Ordering::SeqCst);
    let hits = *fake.hits.lock().unwrap();
    assert!(ctx.supervise().await.is_empty());
    let rows = ctx.c1_push_rows(&seat)?;
    assert_eq!(rows[0].0, Some(1), "{rows:?}");
    assert!(rows[0].1.is_some());
    assert_eq!(*fake.hits.lock().unwrap(), hits + 1);

    // The successor dies with delivery failing again; the Hub restarts before
    // anything retries, and its boot evaluation sends the push.
    let _ = node.next_launch().await?;
    ctx.report_running(&node, &successor, SESSION).await?;
    fake.fail.store(true, Ordering::SeqCst);
    ctx.report_killed(&node, &successor).await?;
    assert_eq!(ctx.supervise().await.len(), 1);
    assert_eq!(ctx.c1_push_rows(&seat)?[1], (None, None));
    drop(node);
    fake.fail.store(false, Ordering::SeqCst);
    ctx.restart_hub_push(&fake, true).await?;
    let rows = ctx.c1_push_rows(&seat)?;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].0, Some(1), "the boot evaluation sent it: {rows:?}");
    Ok(())
}

// ── r2 item 6: the three "no restart" rows of the §6.3 cause table ─────

impl Ctx {
    /// Append `events` to a running seat, run the supervisor, and assert the
    /// row is "none": no decision, no restart-decision row, a lifecycle that
    /// is neither exited nor failed, and no successor launch.
    async fn assert_no_restart(
        &self,
        node: &mut FakeNode,
        seat: &str,
        label: &str,
        events: Vec<Value>,
    ) -> Result<()> {
        let before: Value = self.instance(seat).await?;
        let before_seq = before["durableSeq"]
            .as_str()
            .unwrap_or("0")
            .parse::<u64>()?;
        let count = u64::try_from(events.len())?;
        for event in events {
            node.appends.send((seat.to_owned(), event))?;
        }
        self.wait_instance(seat, |view| {
            view["durableSeq"]
                .as_str()
                .and_then(|seq| seq.parse::<u64>().ok())
                .is_some_and(|seq| seq >= before_seq + count)
        })
        .await
        .with_context(|| format!("{label}: events journaled"))?;
        let decided = self.supervise().await;
        assert!(decided.is_empty(), "{label}: {decided:?}");
        assert!(self.decisions(seat)?.is_empty(), "{label}: decision row");
        let view = self.instance(seat).await?;
        assert!(
            !matches!(view["lifecycle"].as_str(), Some("exited" | "failed")),
            "{label}: lifecycle {}",
            view["lifecycle"]
        );
        let record = self.lineage(seat).await?;
        assert_eq!(record["generation"], json!(1), "{label}: {record}");
        assert_ne!(record["state"], json!("paused"), "{label}: {record}");
        assert!(
            node.drain().iter().all(|(method, _)| !matches!(
                method.as_str(),
                "instance.create" | "instance.resume"
            )),
            "{label}: no successor launch"
        );
        Ok(())
    }
}

fn turn_event(name: &str, status: &str, severity: &str, related: Value) -> Value {
    json!({"kind": "lifecycle", "payload": {
        "type": "native", "topic": "turn", "nativeName": name,
        "nativeId": {"state": "known", "value": SESSION},
        "status": {"state": "known", "value": status},
        "relatedIds": related, "dataRef": null,
        "severity": severity, "affectsCompletion": false}})
}

/// Row: a settled root-session turn failure — the root turn's result reports
/// an API error (429). The turn ends `failed` (idle plus a turn-error
/// marker); it is not an exit and not a restart cause.
#[tokio::test]
async fn a_settled_root_turn_failure_is_not_a_restart_cause() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.assert_no_restart(
        &mut node,
        &seat,
        "settled API 429",
        vec![
            turn_event(
                "turn_started",
                "working",
                "info",
                json!({"nativeClientMessageId": "m1"}),
            ),
            turn_event(
                "result",
                "error",
                "error",
                json!({"resultIndex": "0", "queuedTurnCount": "0", "settledRootTurn": "true",
                       "lastError": "API Error: 429 rate limited"}),
            ),
        ],
    )
    .await?;
    let view = ctx.instance(&seat).await?;
    assert_eq!(view["activity"], json!("idle"));
    assert_eq!(
        view["lastTurnError"]["text"],
        json!("API Error: 429 rate limited")
    );
    Ok(())
}

/// Row: a main-session StopFailure that §5.6 cannot settle (no `agentId`, no
/// versioned exact native error). Not an exit and not a restart cause.
#[tokio::test]
async fn an_unsettled_main_session_stop_failure_is_not_a_restart_cause() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.assert_no_restart(
        &mut node,
        &seat,
        "unsettled main StopFailure",
        vec![
            turn_event(
                "turn_started",
                "working",
                "info",
                json!({"nativeClientMessageId": "m1"}),
            ),
            turn_event(
                "StopFailure",
                "idle",
                "warning",
                json!({"outcome": "failed", "phase": "turn-ended"}),
            ),
        ],
    )
    .await
}

/// Row: scoped failures — a subagent StopFailure, a workflow member failure,
/// a configure failure and a `severity=error` diagnostic. Each stays in its
/// own scope: not an exit and not a restart cause.
#[tokio::test]
async fn scoped_failures_are_not_a_restart_cause() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let cases = vec![
        (
            "subagent StopFailure",
            turn_event(
                "StopFailure",
                "idle",
                "warning",
                json!({"agentId": "agent0sub0agent000", "agentType": "workflow-subagent",
                       "outcome": "failed", "phase": "turn-ended"}),
            ),
        ),
        (
            "subagent session error",
            json!({"kind": "lifecycle", "payload": {
                "type": "native", "topic": "session", "nativeName": "error",
                "severity": "error", "affectsCompletion": true,
                "status": {"state": "known", "value": "error"},
                "relatedIds": {"agentId": "x", "agentType": "workflow-subagent"}}}),
        ),
        (
            "workflow member failed",
            json!({"kind": "lifecycle", "payload": {
                "type": "entity", "entityType": "workflow.member",
                "state": "failed", "reasonCode": "member-stop-failed",
                "memberId": "wf_owner:agent:agent0sub0agent000"}}),
        ),
        (
            "configure failure",
            json!({"kind": "lifecycle", "payload": {
                "type": "native", "topic": "configuration", "nativeName": "instance.configure",
                "severity": "error", "affectsCompletion": false,
                "status": {"state": "known", "value": "model-control-unavailable:x"}}}),
        ),
        (
            "severity=error diagnostic",
            json!({"kind": "lifecycle", "payload": {
                "type": "native", "topic": "diagnostic", "nativeName": "api_error",
                "severity": "error", "affectsCompletion": false,
                "status": {"state": "known", "value": "rate limited"}}}),
        ),
    ];
    for (label, event) in cases {
        ctx.assert_no_restart(&mut node, &seat, label, vec![event])
            .await?;
    }
    Ok(())
}

// ── r3: the boot evaluation decides history as stale-at-boot ───────────

/// The first boot after an upgrade must not relaunch long-dead seats. An end
/// the Hub first saw more than 30 minutes before its previous run was last
/// alive is decided `stale-at-boot` by the boot evaluation, without F: no
/// fence, no successor, no push. Only the first-seen column is aged; the
/// journal is untouched. A recent end, decided by the same boot evaluation,
/// restarts as usual.
#[tokio::test]
async fn the_boot_evaluation_records_old_deaths_without_relaunching_them() -> Result<()> {
    let mut ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let old = ctx.seat(&mut node).await?;
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
            "title": "Helper", "permissionMode": "auto", "model": "claude-test-model",
            "restart": {"onProcessLoss": true, "maxPerHour": 3}, "prompt": "helper brief",
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let recent = created["instance"]["instanceId"]
        .as_str()
        .context("recent")?
        .to_owned();
    let _ = node.next_launch().await?;
    for id in [&old, &recent] {
        ctx.report_running(&node, id, SESSION).await?;
        ctx.report_killed(&node, id).await?;
    }
    // The Hub first saw the old seat's end long before its run ended.
    ctx.db()?.execute(
        "UPDATE instances SET end_seen_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![old],
    )?;
    let token = node.node_token.clone().context("nodeToken")?;
    drop(node);
    ctx.restart_hub(true).await?;

    let rows = ctx.decisions(&old)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2, "stale-at-boot", "{rows:?}");
    assert_eq!(rows[0].3, None, "no successor");
    let record = ctx.lineage(&old).await?;
    assert_eq!(record["generation"], json!(1), "no F: {record}");
    assert_eq!(record["fences"], json!([]), "no fence: {record}");
    assert_eq!(record["chapters"].as_array().map(Vec::len), Some(1));
    let pushes: i64 = ctx.db()?.query_row(
        "SELECT COUNT(*) FROM lineage_c1_pushes WHERE lineage_id = ?1",
        rusqlite::params![old],
        |row| row.get(0),
    )?;
    assert_eq!(pushes, 0, "no push for an old death");
    // The recent end was decided by the same boot evaluation.
    let rows = ctx.decisions(&recent)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2, "restarted");
    // The returning Node gets the recent lineage's fence and successor only.
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, true, json!({})).await?;
    let (method, create) = node.next_launch().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["lineage"]["id"], json!(recent));
    // The stale seat is still resumable by its owner.
    let resumed = ctx.resume(&old).await?;
    assert_eq!(resumed.status(), 200, "{}", resumed.text().await?);
    Ok(())
}

// ── r2 item 8: a notice whose successor create was rejected ─────────────

/// The successor's host rejects its create (a non-fence reason): the restart
/// notice can never follow it, so the next evaluation settles the notice
/// `cancelled` with reason `successor-create-rejected` instead of leaving it
/// queued forever. It is never sent.
#[tokio::test]
async fn a_notice_whose_successor_create_was_rejected_is_cancelled() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    node.errors.lock().unwrap().insert(
        "instance.resume".into(),
        (-32000, "resume refused: the native session is gone".into()),
    );
    ctx.report_killed(&node, &seat).await?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    let record = ctx.lineage(&seat).await?;
    let notice = record["restartDecisions"][0]["noticeCommandId"]
        .as_str()
        .context("notice")?
        .to_owned();
    let create_state: (String, Option<String>) = ctx.db()?.query_row(
        "SELECT c.state, c.settlement_outcome FROM commands c
           JOIN lineage_restart_decisions d ON d.create_command_id = c.id
          WHERE d.failed_instance_id = ?1",
        rusqlite::params![seat],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(create_state, ("settled".into(), Some("rejected".into())));
    // The next evaluation settles the orphaned notice.
    ctx.supervise().await;
    let row: (String, Option<String>, Option<String>, bool) = ctx.db()?.query_row(
        "SELECT state, settlement_outcome, settlement_reason, forwarded FROM commands WHERE id = ?1",
        rusqlite::params![notice],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(
        row,
        (
            "settled".into(),
            Some("cancelled".into()),
            Some("successor-create-rejected".into()),
            false
        )
    );
    assert!(
        node.drain()
            .iter()
            .all(|(method, _)| method != "instance.send"),
        "the notice is never sent"
    );
    Ok(())
}

// ── r3: when the Hub first saw a chapter's end ──────────────────────────

impl Ctx {
    fn end_seen_at(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db()?.query_row(
            "SELECT end_seen_at FROM instances WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )?)
    }
}

/// The first time the Hub holds process-end evidence for a chapter it stamps
/// `end_seen_at` with its own clock, once: a later end event does not move
/// it, whatever that event's own `observedAt` says.
#[tokio::test]
async fn the_hub_stamps_when_it_first_saw_an_end_once() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    assert_eq!(ctx.end_seen_at(&seat)?, None, "a live chapter has none");
    let before = chrono_now();
    ctx.report_killed(&node, &seat).await?;
    let first = ctx.end_seen_at(&seat)?.context("stamped at first sight")?;
    assert!(
        first >= before,
        "Hub clock, not the event's observedAt: {first} < {before}"
    );
    // A second end report arrives later: the first sighting stands.
    let view = ctx.instance(&seat).await?;
    let seq = view["durableSeq"].as_str().unwrap_or("0").parse::<u64>()?;
    node.appends.send((
        seat.clone(),
        json!({"kind":"lifecycle","observedAt":"2026-11-01T09:00:00.000Z","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"exited"},"severity":"info",
            "affectsCompletion":true,"relatedIds":{}}}),
    ))?;
    ctx.wait_instance(&seat, |view| {
        view["durableSeq"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value > seq)
    })
    .await?;
    assert_eq!(ctx.end_seen_at(&seat)?.as_deref(), Some(first.as_str()));
    Ok(())
}

/// Hub-clock now in the RFC3339 shape the Hub writes.
fn chrono_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

// ── r3: bounded retry, then a visible pause ─────────────────────────────

/// RFC3339 `minutes` (and `seconds`) before now, in the Hub's shape.
fn ago(minutes: i64, seconds: i64) -> String {
    let at = time::OffsetDateTime::now_utc()
        - time::Duration::minutes(minutes)
        - time::Duration::seconds(seconds);
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

impl Ctx {
    /// Age when the Hub first saw `id`'s end (only that column).
    fn age_end_seen(&self, id: &str, at: &str) -> Result<()> {
        let changed = self.db()?.execute(
            "UPDATE instances SET end_seen_at = ?2 WHERE id = ?1",
            rusqlite::params![id, at],
        )?;
        anyhow::ensure!(changed == 1, "no end_seen_at to age for {id}");
        Ok(())
    }

    /// A gateway provider profile; `delivery` as the operator sets it.
    async fn gateway(&self, delivery: Option<Value>) -> Result<String> {
        let mut body = json!({
            "name": "routed", "kind": "gateway", "baseUrl": "http://127.0.0.1:1",
            "models": ["passthrough/auto"], "authToken": "sk-fake-zzzz",
        });
        if let Some(delivery) = delivery {
            body["delivery"] = delivery;
        }
        let created: Value = self
            .http
            .post(format!("{}/v1/providers", self.base()))
            .bearer_auth(&self.human)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(created["id"].as_str().context("profile id")?.to_owned())
    }

    /// A Main seat launched with provider profile `profile`.
    async fn seat_on_profile(&self, node: &mut FakeNode, profile: &str) -> Result<String> {
        let created = self
            .http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(&self.human)
            .json(&json!({
                "hostId": self.host, "kind": "claude", "driver": "claude-sdk",
                "title": "Main", "grants": ["address-owner"], "permissionMode": "auto",
                "model": "passthrough/auto", "providerProfileId": profile,
                "restart": {"onProcessLoss": true, "maxPerHour": 3}, "prompt": "seat brief",
            }))
            .send()
            .await?;
        let status = created.status();
        let created: Value = created.json().await?;
        anyhow::ensure!(status.is_success(), "seat on {profile}: {status} {created}");
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("seat")?
            .to_owned();
        let _ = node.next_launch().await?;
        Ok(id)
    }

    /// `(kind, title)` of every C1 push recorded for a lineage.
    fn c1_push_kinds(&self, lineage: &str) -> Result<Vec<(String, String)>> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT kind, title FROM lineage_c1_pushes WHERE lineage_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![lineage], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The lineage is still undecided: no decision row, generation 1, not
    /// paused, no fence, no successor launch.
    async fn assert_undecided(&self, node: &mut FakeNode, seat: &str, label: &str) -> Result<()> {
        assert!(self.decisions(seat)?.is_empty(), "{label}: decision row");
        let record = self.lineage(seat).await?;
        assert_eq!(record["generation"], json!(1), "{label}: {record}");
        assert_ne!(record["state"], json!("paused"), "{label}: {record}");
        assert_eq!(record["fences"], json!([]), "{label}: {record}");
        assert!(
            node.drain().iter().all(|(method, _)| !matches!(
                method.as_str(),
                "instance.create" | "instance.resume"
            )),
            "{label}: no successor launch"
        );
        Ok(())
    }
}

/// A candidate whose successor preparation fails transiently is retried on
/// every tick inside the window (first seen 29 minutes ago here), decides
/// nothing meanwhile, and restarts once preparation succeeds.
#[tokio::test]
async fn a_transient_prepare_error_retries_within_the_window_then_restarts() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    ctx.age_end_seen(&seat, &ago(29, 0))?;
    ctx.hub.test_handle().test_fail_restart_prepares(3);
    for tick in 0..3 {
        assert!(ctx.supervise().await.is_empty(), "tick {tick}");
        ctx.assert_undecided(&mut node, &seat, &format!("tick {tick}"))
            .await?;
    }
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["outcome"], json!("restarted"));
    let (method, _) = node.next_launch().await?;
    assert_eq!(method, "instance.resume");
    assert!(
        ctx.c1_push_kinds(&seat)?
            .iter()
            .all(|(kind, _)| kind == "restarted")
    );
    Ok(())
}

/// A successor that never becomes preparable — its provider profile is gone
/// and nobody re-creates it — decides nothing until the window closes, then
/// is paused `paused{process-exit}` with the restart-failed push, not before.
#[tokio::test]
async fn a_candidate_that_never_recovers_pauses_with_the_push_at_the_window_boundary() -> Result<()>
{
    let (ctx, fake) = Ctx::boot_push().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let gone = format!("prv_{}", uuid::Uuid::now_v7());
    ctx.db()?.execute(
        "UPDATE instances SET spec_json = json_set(spec_json, '$.providerProfileId', ?2)
          WHERE id = ?1",
        rusqlite::params![seat, gone],
    )?;
    ctx.report_killed(&node, &seat).await?;
    // Inside the window: retried, nothing decided, no push.
    ctx.age_end_seen(&seat, &ago(29, 0))?;
    for tick in 0..2 {
        assert!(ctx.supervise().await.is_empty(), "tick {tick}");
        ctx.assert_undecided(&mut node, &seat, &format!("tick {tick}"))
            .await?;
    }
    assert!(
        ctx.c1_push_kinds(&seat)?.is_empty(),
        "no push before the boundary"
    );
    // At the boundary: paused with the restart-failed push.
    let hits = *fake.hits.lock().unwrap();
    ctx.age_end_seen(&seat, &ago(30, 5))?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["outcome"], json!("process-exit"));
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["pausedBy"]["kind"], json!("process-exit"));
    let reason = record["restartDecisions"][0]["reason"]
        .as_str()
        .context("reason")?;
    assert!(reason.contains("30 minutes"), "{reason}");
    assert_eq!(
        ctx.c1_push_kinds(&seat)?,
        [(
            "restart-failed".to_owned(),
            "Main paused: could not restart".to_owned()
        )]
    );
    assert_eq!(*fake.hits.lock().unwrap(), hits + 1);
    Ok(())
}

/// A missing provider profile is not permanent: a profile deleted and then
/// re-created (same id, restored) within the window restarts the seat.
#[tokio::test]
async fn a_deleted_profile_recreated_within_the_window_restarts_the_seat() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let profile = ctx.gateway(None).await?;
    let seat = ctx.seat_on_profile(&mut node, &profile).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    let db = ctx.db()?;
    db.execute(
        "CREATE TABLE restored_profiles AS SELECT * FROM provider_profiles WHERE id = ?1",
        rusqlite::params![profile],
    )?;
    db.execute(
        "DELETE FROM provider_profiles WHERE id = ?1",
        rusqlite::params![profile],
    )?;
    drop(db);
    ctx.report_killed(&node, &seat).await?;
    for tick in 0..2 {
        assert!(ctx.supervise().await.is_empty(), "tick {tick}");
        ctx.assert_undecided(&mut node, &seat, &format!("tick {tick}"))
            .await?;
    }
    ctx.db()?.execute(
        "INSERT INTO provider_profiles SELECT * FROM restored_profiles",
        [],
    )?;
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["outcome"], json!("restarted"));
    let (method, create) = node.next_launch().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(create["spec"]["providerProfileId"], json!(profile));
    Ok(())
}

impl Ctx {
    /// A worker host and a proxy host, both relay-capable, and a gateway
    /// profile delivered via the proxy. Returns (worker node, proxy host,
    /// profile).
    async fn via_setup(&mut self) -> Result<(FakeNode, String, String)> {
        let relay = json!({"capabilities": {"lineageFence": true, "apiRelay": true}});
        let node = FakeNode::connect_with(
            &self.hub,
            &self.host,
            &self.hub.mint_enroll_token(5).await?,
            true,
            relay.clone(),
        )
        .await?;
        let proxy = HostId::new().as_id().as_str().to_owned();
        let proxy_node = FakeNode::connect_with(
            &self.hub,
            &proxy,
            &self.hub.mint_enroll_token(5).await?,
            true,
            relay,
        )
        .await?;
        // The proxy link stays up for the launch; the test drops the row.
        std::mem::forget(proxy_node);
        let profile = self
            .gateway(Some(
                json!({"mode": "via", "viaHostId": proxy, "route": "hub-relay"}),
            ))
            .await?;
        Ok((node, proxy, profile))
    }
}

/// The profile's own via delivery names a proxy host whose row is gone
/// (`resolve_and_attach` → `api-via-unknown-host`): permanent, so the
/// supervisor pauses at once with the restart-failed push.
#[tokio::test]
async fn a_via_delivery_profile_whose_proxy_host_is_gone_pauses_at_once() -> Result<()> {
    let (mut ctx, fake) = Ctx::boot_push().await?;
    let (mut node, proxy, profile) = ctx.via_setup().await?;
    let seat = ctx.seat_on_profile(&mut node, &profile).await?;
    let view = ctx.instance(&seat).await?;
    assert_eq!(
        view["apiRoute"]["viaHostId"],
        json!(proxy),
        "{}",
        view["apiRoute"]
    );
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    ctx.db()?
        .execute("DELETE FROM hosts WHERE id = ?1", rusqlite::params![proxy])?;
    let hits = *fake.hits.lock().unwrap();
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["outcome"], json!("process-exit"));
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["pausedBy"]["kind"], json!("process-exit"));
    let reason = record["restartDecisions"][0]["reason"]
        .as_str()
        .context("reason")?;
    assert!(reason.contains(&proxy), "{reason}");
    assert_eq!(
        ctx.c1_push_kinds(&seat)?,
        [(
            "restart-failed".to_owned(),
            "Main paused: could not restart".to_owned()
        )]
    );
    assert_eq!(*fake.hits.lock().unwrap(), hits + 1);
    let _ = &mut ctx;
    Ok(())
}

/// Owner Resume keeps the structured refusal: when the chapter's observed via
/// route names a proxy host that is gone, the Resume answers 400 with code
/// `api-via-unknown-host`, not a generic bad request.
#[tokio::test]
async fn owner_resume_keeps_the_api_via_unknown_host_code() -> Result<()> {
    let mut ctx = Ctx::boot().await?;
    let (mut node, proxy, profile) = ctx.via_setup().await?;
    let seat = ctx.seat_on_profile(&mut node, &profile).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    assert_eq!(ctx.close(&seat).await?.status(), 200);
    // The operator moved the profile to direct delivery and retired the
    // proxy; the chapter still observed the via route.
    let db = ctx.db()?;
    db.execute(
        "UPDATE provider_profiles SET delivery_json = '{\"mode\":\"direct\",\"route\":\"auto\"}'
          WHERE id = ?1",
        rusqlite::params![profile],
    )?;
    db.execute("DELETE FROM hosts WHERE id = ?1", rusqlite::params![proxy])?;
    drop(db);
    let resumed = ctx.resume(&seat).await?;
    let status = resumed.status();
    let body: Value = resumed.json().await?;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], json!("api-via-unknown-host"), "{body}");
    Ok(())
}

// ── r4 item 1: the run marker ───────────────────────────────────────────

impl Ctx {
    fn marker(&self) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT last_alive_at FROM hub_run_marker WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }
}

/// The boot evaluation measures history from the previous run's marker, not
/// from its own clock. The previous run was last alive 10 minutes ago and
/// first saw the end 35 minutes ago: the marker puts the history line at 40
/// minutes ago, so the death is decided normally, whereas the no-marker
/// fallback (30 minutes before boot) would wrongly call it stale. Only the
/// marker and the first-seen column are aged. With the supervisor off, the
/// first tick writes the marker; the new run writes its own after the boot
/// read.
#[tokio::test]
async fn the_boot_reads_the_previous_run_marker_before_writing_its_own() -> Result<()> {
    let mut ctx = Ctx::boot().await?;
    // restart_supervisor = false here: the tick still writes the marker.
    let first = ctx
        .wait_until(|| async { ctx.marker().ok().flatten() })
        .await
        .context("the first tick writes a marker row")?;
    assert!(!first.is_empty());
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    let token = node.node_token.clone().context("nodeToken")?;
    drop(node);
    let (alive, seen) = (ago(10, 0), ago(35, 0));
    ctx.restart_hub_editing(true, |db| {
        db.execute(
            "UPDATE hub_run_marker SET last_alive_at = ?1 WHERE id = 1",
            rusqlite::params![alive],
        )?;
        db.execute(
            "UPDATE instances SET end_seen_at = ?2 WHERE id = ?1",
            rusqlite::params![seat, seen],
        )?;
        Ok(())
    })
    .await?;
    let rows = ctx.decisions(&seat)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2, "restarted", "decided normally: {rows:?}");
    let mut node = FakeNode::connect_with(&ctx.hub, &ctx.host, &token, true, json!({})).await?;
    let (method, _) = node.next_launch().await?;
    assert_eq!(method, "instance.resume");
    // The new run has written its own marker since.
    let now_marker = ctx
        .wait_until(|| async { ctx.marker().ok().flatten().filter(|at| at > &alive) })
        .await
        .context("the new run writes its own marker")?;
    assert!(now_marker > alive);
    Ok(())
}

// ── r4 item 2: failures before preparation follow the same rule ─────────

/// The failed chapter's host row is gone: its successor can never be placed,
/// so the supervisor pauses at once with the restart-failed push instead of
/// retrying silently on every tick.
#[tokio::test]
async fn a_seat_whose_host_row_is_gone_pauses_at_once_with_the_push() -> Result<()> {
    let (ctx, fake) = Ctx::boot_push().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    node.disconnect();
    ctx.db()?.execute(
        "DELETE FROM hosts WHERE id = ?1",
        rusqlite::params![ctx.host],
    )?;
    let hits = *fake.hits.lock().unwrap();
    let decided = ctx.supervise().await;
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["outcome"], json!("process-exit"));
    let record = ctx.lineage(&seat).await?;
    assert_eq!(record["pausedBy"]["kind"], json!("process-exit"));
    let reason = record["restartDecisions"][0]["reason"]
        .as_str()
        .context("reason")?;
    assert!(reason.contains(&ctx.host), "{reason}");
    assert_eq!(
        ctx.c1_push_kinds(&seat)?,
        [(
            "restart-failed".to_owned(),
            "Main paused: could not restart".to_owned()
        )]
    );
    assert_eq!(*fake.hits.lock().unwrap(), hits + 1);
    Ok(())
}

/// A lookup error before preparation — loading the candidate, counting the
/// hour's restarts, or the host and recovery-source lookups — is retryable
/// under the same bound: nothing is decided inside the window, and at the
/// boundary the lineage is paused with the restart-failed push.
#[tokio::test]
async fn a_lookup_error_before_preparation_pauses_at_the_window_boundary() -> Result<()> {
    for point in ["pending", "cap", "recovery"] {
        let (ctx, fake) = Ctx::boot_push().await?;
        let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
        let seat = ctx.seat(&mut node).await?;
        ctx.report_running(&node, &seat, SESSION).await?;
        ctx.report_killed(&node, &seat).await?;
        ctx.hub.test_handle().test_fail_restart_step(point, 1_000);
        ctx.age_end_seen(&seat, &ago(29, 0))?;
        for tick in 0..2 {
            assert!(ctx.supervise().await.is_empty(), "{point} tick {tick}");
            ctx.assert_undecided(&mut node, &seat, &format!("{point} tick {tick}"))
                .await?;
        }
        assert!(ctx.c1_push_kinds(&seat)?.is_empty(), "{point}: no push yet");
        let hits = *fake.hits.lock().unwrap();
        ctx.age_end_seen(&seat, &ago(30, 5))?;
        ctx.supervise().await;
        let record = ctx.lineage(&seat).await?;
        assert_eq!(record["state"], json!("paused"), "{point}: {record}");
        assert_eq!(record["pausedBy"]["kind"], json!("process-exit"), "{point}");
        let reason = record["restartDecisions"][0]["reason"]
            .as_str()
            .context("reason")?;
        assert!(
            reason.contains(&format!("injected transient store error at {point}")),
            "{reason}"
        );
        assert_eq!(
            ctx.c1_push_kinds(&seat)?,
            [(
                "restart-failed".to_owned(),
                "Main paused: could not restart".to_owned()
            )],
            "{point}"
        );
        assert_eq!(*fake.hits.lock().unwrap(), hits + 1, "{point}");
    }
    Ok(())
}

// ── r4 item 3: an unreadable marker is an error, not "no previous run" ──

/// A marker the boot cannot read is not guessed as "no previous run": the
/// read error is logged and this boot skips the stale classification, so an
/// end first seen two hours ago — which the no-marker fallback would call
/// stale — is decided normally.
#[tokio::test]
async fn an_unreadable_marker_skips_stale_classification_at_boot() -> Result<()> {
    let mut ctx = Ctx::boot().await?;
    ctx.wait_until(|| async { ctx.marker().ok().flatten() })
        .await
        .context("marker row")?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    ctx.report_killed(&node, &seat).await?;
    drop(node);
    let seen = ago(120, 0);
    ctx.restart_hub_editing(true, |db| {
        db.execute(
            "UPDATE hub_run_marker SET last_alive_at = X'00' WHERE id = 1",
            [],
        )?;
        db.execute(
            "UPDATE instances SET end_seen_at = ?2 WHERE id = ?1",
            rusqlite::params![seat, seen],
        )?;
        Ok(())
    })
    .await?;
    let rows = ctx.decisions(&seat)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2, "restarted", "no guess at boot: {rows:?}");
    Ok(())
}

// ── r4 item 4: owner Resume of a missing profile keeps main's shape ─────

/// Owner Resume of a lineage whose provider profile is gone answers exactly
/// as it did before ma-restart: `resolve_and_attach`'s refusal, 422
/// `PLACEMENT_UNSATISFIABLE` naming the unknown profile. (Round 2's pre-check
/// had turned it into 400 `BAD_REQUEST`; the supervisor's retry rule does
/// not change the owner-facing answer.)
#[tokio::test]
async fn owner_resume_of_a_missing_profile_keeps_the_pre_restart_shape() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true).await?;
    let seat = ctx.seat(&mut node).await?;
    ctx.report_running(&node, &seat, SESSION).await?;
    assert_eq!(ctx.close(&seat).await?.status(), 200);
    let gone = format!("prv_{}", uuid::Uuid::now_v7());
    ctx.db()?.execute(
        "UPDATE instances SET spec_json = json_set(spec_json, '$.providerProfileId', ?2)
          WHERE id = ?1",
        rusqlite::params![seat, gone],
    )?;
    let resumed = ctx.resume(&seat).await?;
    let status = resumed.status();
    let body: Value = resumed.json().await?;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], json!("PLACEMENT_UNSATISFIABLE"), "{body}");
    assert!(body.to_string().contains(&gone), "{body}");
    Ok(())
}
