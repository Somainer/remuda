//! D-057 ma-lineage: lineages, continuation resume and lineage-aware edges.
//!
//! Every case drives the Hub over HTTP with Agent/Human tokens and one or
//! more scripted fake Nodes. Cases that prove the fix fail on origin/main:
//! there the resume copies no delegation (the successor holds no grants) and
//! `owns()` is one physical hop.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8000-0000000000aa";

/// Journal event a Node emits when a driver reports the session it is running.
fn session_started(session_id: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": "session",
            "nativeId": { "state": "known", "value": session_id },
            "status": { "state": "known", "value": "started" },
            "relatedIds": {},
            "dataRef": null,
            "severity": "info",
            "affectsCompletion": false
        }
    })
}

fn exited() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" }
    })
}

/// The REAL print/SDK process-exit event the driver's `emit_exit` produces:
/// topic=session, nativeName=session, status=exited, severity INFO (the
/// ma-lineage r4 item 2 SDK/print shape — a clean exit, never a failure).
fn sdk_session_exited(observed_at: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "observedAt": observed_at,
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": "session",
            "severity": "info",
            "affectsCompletion": false,
            "status": { "state": "known", "value": "exited" },
            "relatedIds": {}
        }
    })
}

/// The REAL shell/generic PTY process-exit event (shell_pty NATIVE_EXIT):
/// nativeName=native_exit. A zero exit / INFO severity is a clean EXIT;
/// `exit_code=1` makes it a failure.
fn pty_native_exit(observed_at: &str, exit_code: u16) -> Value {
    let (status, severity) = if exit_code == 0 {
        ("exited", "info")
    } else {
        ("failed", "error")
    };
    json!({
        "kind": "lifecycle",
        "observedAt": observed_at,
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": "native_exit",
            "severity": severity,
            "affectsCompletion": false,
            "status": { "state": "known", "value": status },
            "relatedIds": {
                "exitCode": exit_code.to_string(),
                "reason": format!("native-exit-code-{exit_code}")
            }
        }
    })
}

struct FakeNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl FakeNode {
    async fn connect_bearer(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
    ) -> Result<(Self, Option<String>)> {
        Self::connect_bearer_hello(hub, host, bearer, json!({})).await
    }

    /// Like [`Self::connect_bearer`] but merges `extra` into the hello
    /// params — e.g. `nodeEpoch` + an `instances` inventory that drives the
    /// node-restart reconcile (ma-lineage r7 item 5c).
    async fn connect_bearer_hello(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        extra: Value,
    ) -> Result<(Self, Option<String>)> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut params = json!({ "hostId": host, "host": {
            "hostname":"lineage-node", "workspaces":[], "workspaceRevision":0,
            "herdr":{"path":"/usr/bin/herdr"},
            "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged_in"}]
        }});
        if let (Some(object), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        node.send(Message::Text(
            json!({
                "jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params": params
            })
            .to_string()
            .into(),
        ))
        .await?;
        let hello: Value = serde_json::from_str(&node.next().await.context("hello")??.into_text()?)
            .context("hello json")?;
        let node_token = hello["result"]["nodeToken"].as_str().map(str::to_owned);
        assert!(hello.get("result").is_some(), "hello failed: {hello}");

        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        let (append_tx, mut append_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((instance_id, event)) = append_rx.recv() => {
                        node.send(Message::Text(json!({
                            "jsonrpc":"2.0","id":"append","method":"journal.append",
                            "params":{"instanceId":instance_id,"event":event}
                        }).to_string().into())).await.unwrap();
                    }
                    frame = node.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let frame: Value = match serde_json::from_str(&text) {
                                    Ok(frame) => frame,
                                    Err(_) => continue,
                                };
                                let Some(method) = frame["method"].as_str() else { continue };
                                // Accept every Hub RPC: launches, closes, sends, answers.
                                if frame_tx.send((method.to_owned(), frame["params"].clone())).is_err() {
                                    break;
                                }
                                if node.send(Message::Text(json!({
                                    "jsonrpc":"2.0","id":frame["id"],"result":{"accepted":true}
                                }).to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(Message::Close(_frame))) => break,
                            Some(Ok(_other)) => {}
                            None | Some(Err(_)) => break,
                        }
                    }
                }
            }
        });
        Ok((
            Self {
                frames: frame_rx,
                appends: append_tx,
                task: Some(task),
            },
            node_token,
        ))
    }

    /// Connect presenting an explicit bearer — the `nodeToken` from a prior
    /// hello when re-enrolling the same host after a Hub restart (D-018:
    /// enroll tokens are single-use).
    async fn connect_with_token(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
    ) -> Result<Self> {
        Ok(Self::connect_bearer(hub, host, bearer).await?.0)
    }

    /// Reconnect with a persistent token and extra hello params (a new
    /// `nodeEpoch` + inventory to trigger the node-restart reconcile).
    async fn connect_with_token_hello(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        extra: Value,
    ) -> Result<Self> {
        Ok(Self::connect_bearer_hello(hub, host, bearer, extra)
            .await?
            .0)
    }

    /// Fresh enrollment (new host): enrolls and returns the persistent
    /// `nodeToken` a later reconnect after a Hub restart must present.
    async fn connect(hub: &remuda_hub::RunningHub, host: &str) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        Ok(Self::connect_bearer(hub, host, &enroll).await?.0)
    }

    async fn next_frame(&mut self) -> Result<(String, Value)> {
        tokio::time::timeout(Duration::from_secs(5), self.frames.recv())
            .await?
            .context("frame")
    }

    /// True when no Hub→Node frame is currently queued (ma-lineage r6 item 1:
    /// a refused delete must not have sent stop/return/purge side effects).
    /// Polls for a short grace so an in-flight frame would be caught.
    async fn has_no_pending_frames(&mut self) -> bool {
        tokio::time::timeout(Duration::from_millis(150), self.frames.recv())
            .await
            .is_err()
    }

    /// Drop the Node connection abruptly; the Hub marks its host offline.
    async fn disconnect(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    db_path: std::path::PathBuf,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let human = hub.mint_device_token("lineage-phone").await?;
        let host = HostId::new().as_id().as_str().to_owned();
        let db_path = dir.path().join("data").join("hub.sqlite");
        Ok(Self {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            db_path,
        })
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn restart_on() -> Value {
        json!({ "onProcessLoss": true, "maxPerHour": 3 })
    }

    /// Create a claude-sdk continuity seat holding `address-owner` +
    /// `dispatch` with the C1 restart policy, and return (instanceId,
    /// launchToken).
    async fn seat(&self, node: &mut FakeNode, scope: Option<Value>) -> Result<(String, String)> {
        let mut body = json!({
            "hostId": self.host,
            "kind": "claude",
            "driver": "claude-sdk",
            "name": "main",
            "title": "Main",
            "grants": ["address-owner", "dispatch"],
            "permissionMode": "manual",
            "restart": Self::restart_on(),
            "prompt": "seat brief",
        });
        if let Some(scope) = scope {
            body["scope"] = scope;
        }
        let created: Value = self
            .http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(&self.human)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("seat id")?
            .to_owned();
        let (method, params) = node.next_frame().await?;
        assert_eq!(method, "instance.create");
        let token = params["agentCredential"]["token"]
            .as_str()
            .context("launch token")?
            .to_owned();
        Ok((id, token))
    }

    async fn report_session(&self, node: &FakeNode, id: &str, exit: bool) -> Result<()> {
        node.appends
            .send((id.to_owned(), session_started(SESSION)))?;
        if exit {
            node.appends.send((id.to_owned(), exited()))?;
        }
        self.wait_until(id, |view| {
            view["nativeSessionId"] == json!(SESSION)
                && (!exit || view["lifecycle"] == json!("exited"))
        })
        .await?;
        Ok(())
    }

    async fn wait_until<F>(&self, id: &str, pred: F) -> Result<Value>
    where
        F: Fn(&Value) -> bool,
    {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let view: Value = self
                    .http
                    .get(format!("{}/v1/instances/{id}", self.base()))
                    .bearer_auth(&self.human)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if pred(&view) {
                    return anyhow::Ok(view);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?
    }

    /// Wait until the durable sequence advances past `before` — a deterministic
    /// barrier that a journal append has been projected, replacing fixed sleeps
    /// before NEGATIVE assertions (ma-lineage r5 item 5).
    async fn wait_seq_advances(&self, id: &str, before: &str) -> Result<Value> {
        self.wait_until(id, |v| {
            v["durableSeq"].as_str().is_some_and(|seq| seq != before)
        })
        .await
    }

    async fn wait_host_offline(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let host: Value = self
                    .http
                    .get(format!("{}/v1/hosts/{}", self.base(), self.host))
                    .bearer_auth(&self.human)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if host["online"] == json!(false) {
                    return anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        Ok(())
    }

    async fn resume(&self, id: &str, token: &str) -> Result<reqwest::Response> {
        self.resume_mode(id, token, "structured").await
    }

    async fn resume_mode(&self, id: &str, token: &str, mode: &str) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}/v1/instances/{id}/resume", self.base()))
            .bearer_auth(token)
            .json(&json!({ "mode": mode }))
            .send()
            .await?)
    }

    async fn instance_is_gone(&self, id: &str) -> Result<bool> {
        let status = self
            .http
            .get(format!("{}/v1/instances/{id}", self.base()))
            .bearer_auth(&self.human)
            .send()
            .await?
            .status();
        Ok(status == 404)
    }

    async fn get_instance(&self, id: &str, token: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}/v1/instances/{id}", self.base()))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn mcp_token(&self, id: &str) -> Result<String> {
        let body: Value = self
            .http
            .post(format!("{}/v1/instances/{id}/mcp-token", self.base()))
            .bearer_auth(&self.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(body["token"].as_str().context("mcp token")?.to_owned())
    }

    /// Agent-origin child (manual mode), as a coordinator chapter would
    /// dispatch it. Returns the new instance id.
    async fn agent_child(&self, parent_token: &str, node: &mut FakeNode) -> Result<String> {
        let created: Value = self
            .http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(parent_token)
            .json(&json!({
                "hostId": self.host,
                "kind": "claude",
                "driver": "claude-print",
                "permissionMode": "manual",
                "prompt": "worker brief",
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("child id")?
            .to_owned();
        let (method, _) = node.next_frame().await?;
        assert_eq!(method, "instance.create");
        Ok(id)
    }
}

/// The D-051 switch override is process-global; cases that rely on it hold
/// this lock for their whole life.
static FLAG_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::const_new(()));

struct FlagGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

async fn d051_on() -> FlagGuard {
    let guard = FLAG_LOCK.lock().await;
    remuda_hub::delegated_decisions_test_support::set_global_override(Some(true));
    FlagGuard { _guard: guard }
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        remuda_hub::delegated_decisions_test_support::set_global_override(None);
    }
}

// --- 1. Live owner resume ------------------------------------------------

#[tokio::test]
async fn owner_resume_of_a_live_chapter_fences_it_and_copies_the_delegation() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, x_token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    let mcp = ctx.mcp_token(&x).await?;
    // The transcript-observed effective permission mode the seat actually ran
    // under (r2-7): the successor must inherit it too, not just the requested
    // word.
    let effective = json!({
        "mode": "acceptEdits", "source": "slash",
        "observedAt": "2026-10-05T12:00:00.000Z"
    });
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances
             SET spec_json = json_set(spec_json, '$.permissionEffective', json(?1))
             WHERE id = ?2",
            rusqlite::params![effective.to_string(), x],
        )?;
    }

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(response["replayed"], json!(false));
    let y = response["instance"]["instanceId"]
        .as_str()
        .context("successor id")?
        .to_owned();
    assert_ne!(x, y);

    // The live predecessor is closed first; the successor's resume create
    // follows.
    let (close_method, close_params) = node.next_frame().await?;
    assert_eq!(close_method, "instance.close");
    assert_eq!(close_params["instanceId"], json!(x));
    let (resume_method, resume_params) = node.next_frame().await?;
    assert_eq!(resume_method, "instance.resume");
    assert_eq!(resume_params["instanceId"], json!(y));
    assert_eq!(resume_params["spec"]["driver"], json!("claude-sdk"));
    assert_eq!(resume_params["spec"]["resumeSessionId"], json!(SESSION));
    assert_eq!(resume_params["spec"]["resumedFrom"], json!(x));
    // r2-7: the seated permission posture — requested and effective —
    // travels to the successor.
    assert_eq!(
        resume_params["spec"]["permissionMode"],
        json!("manual"),
        "successor spec: {}",
        resume_params["spec"]
    );
    assert_eq!(
        resume_params["spec"]["permissionEffective"], effective,
        "successor must inherit the observed effective permission mode"
    );

    // Successor projection: next generation, continuation edge, copied
    // delegation and restart policy.
    let y_view = ctx.get_instance(&y, &ctx.human).await?;
    assert_eq!(y_view["lineageId"], json!(x));
    assert_eq!(y_view["generation"], json!(2));
    assert_eq!(y_view["chapterCause"], json!("owner-resume"));
    assert!(y_view["fencedAt"].is_null());
    assert!(y_view["resumedFrom"].as_str() == Some(x.as_str()));
    assert!(
        y_view["parentInstanceId"].is_null(),
        "continuation edge must not grow depth: successor's parent is the human root"
    );
    assert_eq!(
        y_view["grants"],
        json!(["address-owner", "dispatch"]),
        "successor keeps the predecessor's grants"
    );
    assert_eq!(y_view["restart"], json!(Ctx::restart_on()));
    assert_eq!(y_view["scope"], json!({}), "universe scope projects empty");

    // X is fenced in the same transaction.
    let x_view = ctx.get_instance(&x, &ctx.human).await?;
    assert!(x_view["fencedAt"].is_string());
    assert_eq!(x_view["generation"], json!(1));

    // A second address-owner holder is refused while Y is active: the
    // uniqueness check ignores only fenced chapters.
    let second = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "grants": ["address-owner"], "prompt": "second seat"
        }))
        .send()
        .await?;
    assert_eq!(second.status(), 409);
    assert!(second.text().await?.contains("address-owner"));

    // Both credentials bound to X are revoked immediately.
    for (label, token) in [("launch", x_token), ("mcp", mcp)] {
        let denied = ctx
            .http
            .get(format!("{}/v1/caller", ctx.base()))
            .bearer_auth(token)
            .send()
            .await?;
        assert_eq!(denied.status(), 401, "{label} credential must be revoked");
    }

    // Lineage projection: two chapters, current generation 2.
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["lineageId"], json!(x));
    assert_eq!(lineage["generation"], json!(2));
    assert_eq!(lineage["restart"], json!(Ctx::restart_on()));
    let chapters = lineage["chapters"].as_array().context("chapters")?;
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0]["instanceId"], json!(x));
    assert_eq!(chapters[0]["generation"], json!(1));
    assert!(chapters[0]["fencedAt"].is_string());
    assert_eq!(chapters[1]["instanceId"], json!(y));
    assert_eq!(chapters[1]["chapterCause"], json!("owner-resume"));
    assert!(chapters[1]["fencedAt"].is_null());

    // The successor Agent may read its own lineage but not another one.
    let y_token = resume_params["agentCredential"]["token"]
        .as_str()
        .context("successor launch token")?
        .to_owned();
    let own = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&y_token)
        .send()
        .await?;
    assert_eq!(own.status(), 200);
    // A second continuity lineage (restart policy, no grants) is foreign.
    let other: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "restart": { "onProcessLoss": false, "maxPerHour": 1 },
            "prompt": "other continuity"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let other_id = other["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (_other_method, _) = node.next_frame().await?;
    let foreign_lineage = ctx
        .http
        .get(format!("{}/v1/lineages/{other_id}", ctx.base()))
        .bearer_auth(&y_token)
        .send()
        .await?;
    assert_eq!(foreign_lineage.status(), 403);

    // The successor accepts a second turn.
    let send: reqwest::Response = ctx
        .http
        .post(format!("{}/v1/instances/{y}/commands", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "operation": "instance.send",
            "payload": { "input": { "type": "prompt", "text": "second turn" } }
        }))
        .send()
        .await?;
    assert_eq!(send.status(), 200, "{}", send.text().await?);
    let (send_method, send_params) = loop {
        let frame = node.next_frame().await?;
        if frame.0 == "instance.send" {
            break frame;
        }
    };
    assert_eq!(send_method, "instance.send");
    assert_eq!(send_params["instanceId"], json!(y));

    Ok(())
}

/// D-057 OA6 (round 3): a chapter whose ROOT TURN failed stays RUNNING with a
/// live process (a turn error is never process termination). A continuation
/// MUST close that live predecessor — otherwise two live chapters coexist.
#[tokio::test]
async fn a_live_chapter_after_a_failed_turn_is_closed_when_the_lineage_continues() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    // The chapter reports its native session; a failed turn (429) leaves the
    // row RUNNING — the process is still alive.
    ctx.report_session(&node, &x, false).await?;
    let seq_before = ctx.get_instance(&x, &ctx.human).await?["durableSeq"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native", "topic": "turn", "nativeName": "result",
                "status": { "state": "known", "value": "error" },
                "severity": "info", "affectsCompletion": true,
                "relatedIds": { "queuedTurnCount": "0", "lastError": "api-error: 429" }
            }
        }),
    ))?;
    // Barrier: the result event is applied (durable seq advanced) before the
    // negative assertion.
    ctx.wait_seq_advances(&x, &seq_before).await?;
    let before = ctx.get_instance(&x, &ctx.human).await?;
    assert_eq!(
        before["lifecycle"],
        json!("running"),
        "a failed turn is not process end"
    );

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(x, y);

    // OA6: the failed-but-alive predecessor is closed FIRST...
    let (method, close_params) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    assert_eq!(close_params["instanceId"], json!(x));
    // ...then the successor launches — the predecessor's process cannot still
    // be running beside it.
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    // Only one close was forwarded (exactly one predecessor), and the lineage
    // moved to the successor.
    let duplicate = tokio::time::timeout(Duration::from_millis(150), node.next_frame()).await;
    assert!(
        duplicate.is_err(),
        "only the predecessor close plus the successor create are forwarded"
    );
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["generation"], json!(2));
    Ok(())
}

/// ma-lineage r4 item 1: a LEGACY `failed` row (marked failed by a
/// configure/turn error while the process was alive — no `ended_at`, a
/// non-attestation `last_error`) is POTENTIALLY LIVE. With a known native
/// session the continuation still closes the predecessor before resuming (a
/// genuinely exited row would skip that close); without a session it is
/// refused with 409.
#[tokio::test]
async fn an_ambiguous_legacy_failed_row_with_a_session_is_closed_before_continuation() -> Result<()>
{
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances SET lifecycle = 'failed',
                last_error = 'api-error: 429', ended_at = NULL
             WHERE id = ?1",
            rusqlite::params![x],
        )?;
    }

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        x, y,
        "a known session resumes even from an ambiguous failed row"
    );

    // Potentially live: the predecessor is closed FIRST, then the resume runs.
    let (method, close_params) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    assert_eq!(close_params["instanceId"], json!(x));
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    // The same ambiguous row WITHOUT a session is refused, not relaunched.
    let ctx2 = Ctx::boot().await?;
    let mut node2 = FakeNode::connect(&ctx2.hub, &ctx2.host).await?;
    let (z, _token2) = ctx2.seat(&mut node2, None).await?;
    {
        let db = rusqlite::Connection::open(&ctx2.db_path)?;
        db.execute(
            "UPDATE instances SET lifecycle = 'failed',
                last_error = 'configure failed: model-control-unavailable:x',
                ended_at = NULL
             WHERE id = ?1",
            rusqlite::params![z],
        )?;
    }
    let status = ctx2.resume(&z, &ctx2.human).await?.status();
    assert_eq!(
        status, 409,
        "an ambiguous failed row with no session is potentially live: 409"
    );
    Ok(())
}

/// ma-lineage r4 item 1: a `failed` row WITH recorded end evidence (ended_at
/// stamped) is treated as ended even without an attested-launch marker — the
/// close is skipped and continuation proceeds straight to the resume.
#[tokio::test]
async fn a_failed_row_with_ended_at_is_ended_for_continuation_and_close() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances SET lifecycle = 'failed', ended_at = '2026-09-01T08:30:00.000Z'
             WHERE id = ?1",
            rusqlite::params![x],
        )?;
    }

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_ne!(
        response["instance"]["instanceId"],
        json!(x),
        "the ended failed row continues"
    );
    let (method, _) = node.next_frame().await?;
    assert_eq!(
        method, "instance.resume",
        "an ended predecessor is not closed"
    );
    Ok(())
}

// --- 5. endedAt is the immutable end event -------------------------------

#[tokio::test]
async fn chapter_ended_at_is_the_real_end_event_and_never_tracks_updated_at() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // Real process-end evidence with a fixed event timestamp.
    let ended_at = "2026-09-01T08:30:00.000Z";
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": ended_at,
            "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" }
        }),
    ))?;
    ctx.wait_until(&x, |view| view["lifecycle"] == json!("exited"))
        .await?;

    // A later mutation bumps updated_at (a post-exit command settle, a
    // reconciler pass, ...) but must not move endedAt.
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances SET updated_at = '2026-10-02T09:45:00.000Z' WHERE id = ?1",
            rusqlite::params![x],
        )?;
    }
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let chapter = &lineage["chapters"][0];
    assert_eq!(
        chapter["endedAt"],
        json!(ended_at),
        "endedAt must stay the end-event timestamp, not the mutable updated_at"
    );

    // A second end event with a different timestamp does not rewrite it.
    let seq_before = ctx.get_instance(&x, &ctx.human).await?["durableSeq"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-03T10:00:00.000Z",
            "payload": { "type": "entity", "state": "exited", "reasonCode": "duplicate" }
        }),
    ))?;
    ctx.wait_seq_advances(&x, &seq_before).await?;
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["chapters"][0]["endedAt"], json!(ended_at));

    // Migration backfill (ma-lineage r4 item 3): a terminal row from before
    // the column existed is backfilled from the qualifying process-end EVENT
    // in its journal (that event's observedAt), NOT from updated_at. A later
    // diagnostic (non-terminal, non-liveness) must not veto the backfill.
    let data_dir = ctx._dir.path().to_owned();
    let (human, host) = (ctx.human.clone(), ctx.host.clone());
    let Ctx {
        _dir,
        hub,
        http,
        human: _human,
        host: _host,
        db_path,
    } = ctx;
    hub.shutdown().await;
    {
        let db = rusqlite::Connection::open(&db_path)?;
        // Wipe the immutable column and move updated_at to a DIFFERENT time,
        // so a pass would be obvious if the migration copied it.
        db.execute(
            "UPDATE instances SET ended_at = NULL, updated_at = '2026-09-02T08:30:00.000Z'
             WHERE id = ?1",
            rusqlite::params![x],
        )?;
        // Simulate a pre-r7 database: the journal backfill is the user_version
        // 3 one-time migration.
        db.execute_batch("PRAGMA user_version = 2;")?;
        // Append a later DIAGNOSTIC event (after the exit): it is neither
        // process end nor return-to-live, so the earlier exit must survive.
        db.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, 9001, 'evt_diag_mig',
              '{\"kind\":\"lifecycle\",\"payload\":{\"type\":\"native\",\"topic\":\"diagnostic\",\"nativeName\":\"api_error\",\"status\":{\"state\":\"known\",\"value\":\"rate limited\"},\"severity\":\"error\"}}',
              '2026-09-01T09:00:00.000Z')",
            rusqlite::params![x],
        )?;
    }
    let hub = spawn(HubConfig::for_test(data_dir.join("data"))).await?;
    let db = rusqlite::Connection::open(&db_path)?;
    let stamped: String = db.query_row(
        "SELECT ended_at FROM instances WHERE id = ?1",
        rusqlite::params![x],
        |row| row.get(0),
    )?;
    assert_eq!(
        stamped, ended_at,
        "the migration backfills from the qualifying end EVENT, not updated_at; a later diagnostic must not veto it"
    );
    let _ = (hub, http, human, host, _dir);
    Ok(())
}

/// ma-lineage r4 item 3: the journal backfill respects LATER return-to-live
/// evidence. A terminal row with ended_at NULL whose journal shows an exit
/// and then a later `ready` is NOT backfilled (the process came back); a row
/// with no qualifying end event at all also stays NULL.
#[tokio::test]
async fn ended_at_backfill_skips_an_end_followed_by_return_to_live() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    // Veto case: a row that is exited now, but its journal shows an exit then
    // a later ready — the process returned to live, so no end is backfilled.
    ctx.report_session(&node, &x, false).await?;
    node.appends
        .send((x.clone(), sdk_session_exited("2026-10-01T08:00:00.000Z")))?;
    ctx.wait_until(&x, |v| v["lifecycle"] == json!("exited"))
        .await?;
    // Unknown case: a second row with no end event at all in its journal.
    let (z, _token2) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &z, false).await?;

    let data_dir = ctx._dir.path().to_owned();
    let (human, host) = (ctx.human.clone(), ctx.host.clone());
    let Ctx {
        _dir,
        hub,
        http: _http,
        human: _human,
        host: _host,
        db_path,
    } = ctx;
    hub.shutdown().await;
    {
        let db = rusqlite::Connection::open(&db_path)?;
        // Simulate a pre-r7 database so the reopen runs the one-time journal
        // backfill (user_version 3).
        db.execute_batch("PRAGMA user_version = 2;")?;
        // Wipe both ended_at and append a LATER ready to x's journal.
        db.execute("UPDATE instances SET ended_at = NULL", rusqlite::params![])?;
        db.execute(
            r#"INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, 9100, 'evt_ready_after_exit',
              '{"kind":"lifecycle","payload":{"type":"entity","entityType":"instance","state":"ready","reasonCode":"driver-started"}}',
              '2026-10-01T09:00:00.000Z')"#,
            rusqlite::params![x],
        )?;
        // z carries only session lifecycle frames (started), no end event.
    }
    let _hub = spawn(HubConfig::for_test(data_dir.join("data"))).await?;
    let db = rusqlite::Connection::open(&db_path)?;
    let ended_x: Option<String> = db.query_row(
        "SELECT ended_at FROM instances WHERE id = ?1",
        rusqlite::params![x],
        |row| row.get(0),
    )?;
    assert_eq!(
        ended_x, None,
        "a later return-to-live vetoes the ended_at backfill"
    );
    let ended_z: Option<String> = db.query_row(
        "SELECT ended_at FROM instances WHERE id = ?1",
        rusqlite::params![z],
        |row| row.get(0),
    )?;
    assert_eq!(
        ended_z, None,
        "no qualifying end event leaves ended_at NULL"
    );
    let _ = (human, host, _dir);
    Ok(())
}

// --- 1a. Host loss is contact loss, never process end (ma-lineage r5 item 1)

/// A seat whose HOST link is lost past grace is marked exited+host-lost but
/// keeps no ended_at (D-019: the process may still be running). When the same
/// host reconnects with the node alive, continuing the seat closes the
/// predecessor before resuming, and the chapter's endedAt stays null
/// throughout.
#[tokio::test]
async fn a_host_lost_live_seat_is_closed_before_resume_and_keeps_no_ended_at() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let node_token = node_token.context("nodeToken")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // ma-lineage r6 item 6: drive the real disconnect → host-offline path (no
    // hand-edited hosts table), then the host-lost sweep at grace 0.
    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;
    let (changed, _settlement) = ctx.hub.store().expect("store").expire_lost_hosts(0).await?;
    assert_eq!(changed, 1, "the seat's row is host-lost");
    let row = ctx.get_instance(&x, &ctx.human).await?;
    assert_eq!(row["lifecycle"], json!("exited"), "{row}");
    assert_eq!(row["lastError"], json!("host-contact-lost"), "{row}");
    assert_eq!(
        first_chapter_ended_at(&ctx, &x).await?,
        None,
        "host loss stamps no endedAt: the process may still be running"
    );

    // Reconnect the node with the persistent token (D-018): the link is live
    // again, then continuation closes the possibly-alive predecessor BEFORE
    // the successor resume.
    let mut node = FakeNode::connect_with_token(&ctx.hub, &ctx.host, &node_token).await?;
    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(x, y, "host-lost continues into a new chapter");

    let (method, close_params) = node.next_frame().await?;
    assert_eq!(
        method, "instance.close",
        "the potentially live predecessor is closed"
    );
    assert_eq!(close_params["instanceId"], json!(x));
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    assert_eq!(first_chapter_ended_at(&ctx, &x).await?, None);
    Ok(())
}

/// ma-lineage r6 item 4: X→Y with a session; the host link drops and Y is
/// swept host-lost while Y's process stays alive. A resume addressed to the
/// OLDER chapter X (or the lineage id) must NOT replay dead Y and must run
/// the continuation: instance.close for Y precedes instance.resume, a new
/// successor is minted, and repeating the resume does not replay forever.
#[tokio::test]
async fn a_resume_after_a_host_lost_current_chapter_runs_continuation_not_replay() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let node_token = node_token.context("node token")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, true).await?;

    // Continue X → Y (Y carries the native session).
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    // Y reaches a live lifecycle (the Node launches and initializes it).
    ctx.report_session(&node, &y, false).await?;

    // ma-lineage r7 item 6(b): drop the actual node link instead of flipping
    // hosts.state in SQLite while the socket stays connected.
    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;
    let (changed, _) = ctx.hub.store().expect("store").expire_lost_hosts(0).await?;
    assert_eq!(changed, 1, "Y is host-lost");

    // Reconnect with the persistent node token on the SAME epoch: Y stays the
    // ambiguous (potentially live) host-lost chapter.
    let mut node = FakeNode::connect_with_token(&ctx.hub, &ctx.host, &node_token).await?;

    // Resume addressed to the OLD chapter X while the CURRENT chapter Y is
    // host-lost: continuation, not a {replayed} dead Y.
    let response = ctx.resume(&x, &ctx.human).await?.error_for_status()?;
    assert_eq!(response.status(), 200);
    let third: Value = response.json().await?;
    assert_ne!(
        third["replayed"],
        json!(true),
        "host-lost current chapter is not replayed"
    );
    let z = third["instance"]["instanceId"].as_str().unwrap().to_owned();
    assert_ne!(z, y);

    // The live Y was closed before Z resumed.
    let (method, close_params) = node.next_frame().await?;
    assert_eq!(method, "instance.close", "the potentially live Y is closed");
    assert_eq!(close_params["instanceId"], json!(y));
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    // Repeating the resume addresses the new current Z (requested until the
    // node acks): an idempotent replay is correct here, and it must not send
    // more close frames.
    let replay = ctx.resume(&x, &ctx.human).await?.error_for_status()?;
    assert_eq!(replay.status(), 200);
    let body: Value = replay.json().await?;
    assert_eq!(body["replayed"], json!(true), "the live successor replays");
    assert!(
        node.has_no_pending_frames().await,
        "replaying the live successor sends no new frames"
    );
    Ok(())
}

/// ma-lineage r5 item 2: X→Y, then Y itself ends. A resume still addressed to
/// X must NOT replay the dead Y (`{replayed:true, instance:Y}`); it must
/// continue the lineage from Y — generation 3, an instance.resume for Z with
/// resumedFrom Y.
#[tokio::test]
async fn a_resume_addressed_to_an_old_chapter_after_the_current_ended_continues_it() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, true).await?;

    // First continuation: X → Y.
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    // Report Y's session and then its real SDK exit.
    ctx.report_session(&node, &y, false).await?;
    node.appends
        .send((y.clone(), sdk_session_exited("2026-10-07T08:00:00.000Z")))?;
    ctx.wait_until(&y, |v| v["lifecycle"] == json!("exited"))
        .await?;

    // Resume addressed to the OLDER chapter X now that the current chapter Y
    // has ended: it must mint generation 3 (Z), not replay dead Y.
    let response = ctx.resume(&x, &ctx.human).await?.error_for_status()?;
    assert_eq!(
        response.status(),
        200,
        "an ended current chapter is continued, not refused/replayed"
    );
    let third: Value = response.json().await?;
    assert_ne!(
        third["replayed"],
        json!(true),
        "no idempotent replay of dead Y: {third}"
    );
    let z = third["instance"]["instanceId"].as_str().unwrap().to_owned();
    assert_ne!(z, x);
    assert_ne!(z, y);

    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["generation"], json!(3), "X→Y→Z is generation 3");
    let chapters = lineage["chapters"].as_array().unwrap();
    assert_eq!(chapters.len(), 3, "three chapters X,Y,Z");
    assert_eq!(chapters[2]["instanceId"], json!(z));

    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume", "params={params}");
    assert_eq!(
        params["spec"]["resumedFrom"],
        json!(y),
        "Z continues from Y; params={params}"
    );
    Ok(())
}

// --- 3. Chapter delete: non-current refused, current deletes the lineage --

/// Deleting a NON-CURRENT (predecessor) chapter is refused with NO side
/// effects; deleting the CURRENT chapter removes the WHOLE lineage (every
/// chapter + the lineage row) in one transaction, so nothing is stranded.
#[tokio::test]
async fn deleting_a_non_current_chapter_is_refused_and_current_deletes_lineage() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, true).await?;

    // Continue X → Y (X becomes a closed predecessor chapter).
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (resume_method, _) = node.next_frame().await?;
    assert_eq!(resume_method, "instance.resume");

    // Deleting the predecessor X is refused even though it is terminal.
    let delete_x = ctx
        .http
        .delete(format!("{}/v1/instances/{x}?force=1", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(
        delete_x.status(),
        409,
        "a closed predecessor chapter cannot be deleted"
    );
    // ma-lineage r6 item 1: the refusal runs NO side effects.
    assert!(
        node.has_no_pending_frames().await,
        "deleting a non-current chapter must not send stop/return/purge frames"
    );
    assert!(
        ctx.get_instance(&x, &ctx.human).await?.is_object(),
        "the predecessor chapter row is retained after refusal"
    );

    // Delete the CURRENT chapter Y (it exited via report_session above): the
    // whole lineage is removed in one transaction — X and Y both gone and no
    // lineages row left to strand.
    let delete_y = ctx
        .http
        .delete(format!("{}/v1/instances/{y}?force=1", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(
        delete_y.status(),
        200,
        "the current chapter deletes the lineage"
    );

    assert!(
        ctx.instance_is_gone(&x).await?,
        "predecessor X removed with the lineage"
    );
    assert!(ctx.instance_is_gone(&y).await?, "current Y removed");
    let lineage_status = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .status();
    assert_eq!(lineage_status, 404, "lineage row removed");
    Ok(())
}

/// ma-lineage r6 item 1: a refusal is a pure 409 even for a LIVE predecessor
/// — `?force=1` must not run stop_before_delete (instance.stop) or purge
/// before the lineage-current check.
#[tokio::test]
async fn deleting_a_non_current_live_chapter_refuses_before_any_stop() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // Continue X → Y while X is still live (do not exit it).
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let _y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    // Delete the now non-current (fenced) X with force: 409, no frames beyond
    // the continuation's close/resume already drained.
    let delete_x = ctx
        .http
        .delete(format!("{}/v1/instances/{x}?force=1", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(delete_x.status(), 409);
    assert!(
        node.has_no_pending_frames().await,
        "force-delete of a non-current chapter must not run side effects"
    );
    Ok(())
}

/// ma-lineage r7 item 1: X was continued into Y while X is STILL `running`
/// (the continuation close was accepted but the exit never arrived). A
/// whole-lineage `DELETE Y?force=1` must refuse with 409 in the HTTP
/// pre-check — `?force=1` stops only the addressed chapter, never the other
/// chapters — and must send NOTHING first: no stop for Y, no worktree
/// return, no instance.purge, and no delete audit row.
#[tokio::test]
async fn force_deleting_a_lineage_with_a_live_predecessor_refuses_without_frames() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // Continue X → Y while X stays live (no exit event is appended).
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    let delete_y = ctx
        .http
        .delete(format!("{}/v1/instances/{y}?force=1", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(
        delete_y.status(),
        409,
        "a lineage with a live predecessor refuses even with force=1"
    );
    assert!(
        node.has_no_pending_frames().await,
        "the refusal runs no stop/return/purge side effects"
    );
    // Both rows survive and no delete was audited.
    assert!(ctx.get_instance(&x, &ctx.human).await?.is_object());
    assert!(ctx.get_instance(&y, &ctx.human).await?.is_object());
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    let audits: i64 = db.query_row(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'instance.delete'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(audits, 0, "a refused delete writes no audit row");
    Ok(())
}

/// ma-lineage r7 item 2: deleting the CURRENT chapter removes the WHOLE
/// lineage, so the Hub must ask the Node to purge EVERY chapter's data
/// directory (not only the current one) and the audit must list every
/// deleted chapter.
#[tokio::test]
async fn whole_lineage_delete_purges_and_audits_every_chapter() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, true).await?;

    // Continue X → Y (X terminal). Y is still requested.
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    let delete_y = ctx
        .http
        .delete(format!("{}/v1/instances/{y}?force=1", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?;
    let body: Value = delete_y.json().await?;
    let chapter_ids = body["chapterIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        chapter_ids,
        [x.clone(), y.clone()].into_iter().collect(),
        "the response names every removed chapter: {body}"
    );

    // The only live chapter was Y (requested): force stops it first, then the
    // Node is asked to purge BOTH X and Y (in either order).
    let (method, close_params) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    assert_eq!(close_params["instanceId"], json!(y));
    let mut purged = std::collections::BTreeSet::new();
    while let Ok(Ok((method, params))) =
        tokio::time::timeout(Duration::from_millis(500), node.next_frame()).await
    {
        assert_eq!(
            method, "instance.purge",
            "after the stop only purge frames remain, got {method}"
        );
        purged.insert(params["instanceId"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        purged,
        [x.clone(), y.clone()].into_iter().collect(),
        "every chapter's Node directory is purged"
    );

    // Both rows and the lineage row are gone.
    assert!(ctx.instance_is_gone(&x).await?);
    assert!(ctx.instance_is_gone(&y).await?);
    let lineage_status = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .status();
    assert_eq!(lineage_status, 404);

    // The audit records the whole-lineage delete with every chapter.
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    let detail: String = db.query_row(
        "SELECT detail_json FROM audit_log
         WHERE action = 'instance.delete' AND subject = ?1
         ORDER BY id DESC LIMIT 1",
        rusqlite::params![y],
        |row| row.get(0),
    )?;
    let detail: Value = serde_json::from_str(&detail)?;
    let audited = detail["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| chapter["instanceId"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        audited,
        [x, y].into_iter().collect(),
        "the audit lists every deleted chapter: {detail}"
    );
    Ok(())
}

// --- 7. Ambiguous failed rows keep occupying seat/fan-out (ma-lineage r5) -

/// Mark `id` failed with no ended_at and a plain turn/configure error — the
/// ambiguous failed-but-potentially-live state.
fn mark_ambiguous_failed(db_path: &std::path::Path, id: &str, last_error: &str) -> Result<()> {
    let db = rusqlite::Connection::open(db_path)?;
    db.execute(
        "UPDATE instances SET lifecycle = 'failed', ended_at = NULL,
            fenced_at = NULL, last_error = ?2 WHERE id = ?1",
        rusqlite::params![id, last_error],
    )?;
    Ok(())
}

/// An ambiguous failed address-owner holder (no ended_at, turn-error marker)
/// STILL occupies its seat: a second address-owner create is refused 409. A
/// genuinely-ended failed holder (ended_at stamped) frees the seat.
#[tokio::test]
async fn an_ambiguous_failed_address_owner_holder_still_occupies_the_seat() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;

    mark_ambiguous_failed(&ctx.db_path, &x, "api-error: 429")?;

    let second = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "grants": ["address-owner"], "prompt": "second seat"
        }))
        .send()
        .await?;
    assert_eq!(
        second.status(),
        409,
        "an ambiguous failed holder is potentially live and keeps the seat"
    );

    // Once real end evidence exists the seat is free.
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    db.execute(
        "UPDATE instances SET ended_at = '2026-10-01T08:30:00.000Z' WHERE id = ?1",
        rusqlite::params![x],
    )?;
    drop(db);
    let second = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "grants": ["address-owner"], "prompt": "second seat"
        }))
        .send()
        .await?;
    assert_eq!(
        second.status(),
        200,
        "an evidenced-ended holder frees the seat"
    );
    Ok(())
}

/// Resuming an ambiguous failed holder closes it FIRST (the continuation
/// transaction commits) — it is not dropped by the seat-uniqueness guard
/// before reaching the predecessor close.
#[tokio::test]
async fn resuming_an_ambiguous_failed_holder_closes_it_and_commits() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    mark_ambiguous_failed(
        &ctx.db_path,
        &x,
        "configure failed: model-control-unavailable:x",
    )?;

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let _y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();

    let (method, close_params) = node.next_frame().await?;
    assert_eq!(
        method, "instance.close",
        "the potentially live failed holder is closed"
    );
    assert_eq!(close_params["instanceId"], json!(x));
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    let view = ctx.get_instance(&x, &ctx.human).await?;
    assert!(view["fencedAt"].is_string(), "fence committed: {view}");
    Ok(())
}

/// An ambiguous failed CHILD (no ended_at, a turn error while alive) still
/// occupies its parent's fan-out slot; real end evidence frees it.
#[tokio::test]
async fn an_ambiguous_failed_child_still_counts_toward_fan_out() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;

    let project: Value = ctx
        .http
        .post(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "name": "fanout-ambiguous",
            "policy": { "configurable": { "coordinatorFanOut": 1, "maxDelegationDepth": 2 } }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let project_id = project["id"].as_str().context("project id")?.to_owned();
    let scope = json!({ "projectIds": [project_id] });
    let (x, x_token) = ctx.seat(&mut node, Some(scope)).await?;
    ctx.report_session(&node, &x, false).await?;

    let w1 = ctx.agent_child(&x_token, &mut node).await?;
    mark_ambiguous_failed(&ctx.db_path, &w1, "api-error: 429")?;

    let refused = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&x_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual",
            "scope": { "projectIds": [project_id] },
            "prompt": "would be the second child"
        }))
        .send()
        .await?;
    assert_eq!(
        refused.status(),
        409,
        "ambiguous failed child still holds the slot"
    );
    assert!(refused.text().await?.contains("fan-out"));

    let db = rusqlite::Connection::open(&ctx.db_path)?;
    db.execute(
        "UPDATE instances SET ended_at = '2026-10-01T08:30:00.000Z' WHERE id = ?1",
        rusqlite::params![w1],
    )?;
    drop(db);
    let allowed = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&x_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual",
            "scope": { "projectIds": [project_id] },
            "prompt": "now a valid second child"
        }))
        .send()
        .await?;
    assert_eq!(
        allowed.status(),
        200,
        "an evidenced-ended child frees the slot"
    );
    Ok(())
}

// --- 2. Ended chapter + host offline ------------------------------------

#[tokio::test]
async fn owner_resume_of_an_ended_chapter_works_and_offline_refuses_without_writes() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, true).await?;

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(y, x);
    assert_eq!(
        response["instance"]["grants"],
        json!(["address-owner", "dispatch"])
    );

    // The only frame is the successor resume create: an ended chapter is not
    // closed again.
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["resumeSessionId"], json!(SESSION));
    assert!(
        node.frames.try_recv().is_err(),
        "ended chapter must not receive instance.close"
    );
    let lineage = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(lineage["chapters"].as_array().unwrap().len(), 2);
    assert!(lineage["chapters"][0]["endedAt"].is_string());

    // Host offline on a fresh lineage: 409 and nothing is written.
    let ctx2 = Ctx::boot().await?;
    let mut node2 = FakeNode::connect(&ctx2.hub, &ctx2.host).await?;
    let (z, _) = ctx2.seat(&mut node2, None).await?;
    ctx2.report_session(&node2, &z, true).await?;
    let before = ctx2
        .http
        .get(format!("{}/v1/lineages/{z}", ctx2.base()))
        .bearer_auth(&ctx2.human)
        .send()
        .await?
        .json::<Value>()
        .await?;
    node2.disconnect().await;
    drop(node2);
    ctx2.wait_host_offline().await?;
    let offline = ctx2.resume(&z, &ctx2.human).await?;
    assert_eq!(offline.status(), 409);
    let after = ctx2
        .http
        .get(format!("{}/v1/lineages/{z}", ctx2.base()))
        .bearer_auth(&ctx2.human)
        .send()
        .await?
        .json::<Value>()
        .await?;
    // Structural state is untouched by the refusal (host-offline bookkeeping
    // may refresh chapter timestamps; that is not a resume write).
    for pointer in ["lineageId", "state", "pausedBy", "restart", "generation"] {
        assert_eq!(
            before[pointer], after[pointer],
            "offline refusal changed {pointer}"
        );
    }
    let before_ids: Vec<Value> = before["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| chapter["instanceId"].clone())
        .collect();
    let after_ids: Vec<Value> = after["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| chapter["instanceId"].clone())
        .collect();
    assert_eq!(before_ids, after_ids, "offline refusal created no chapter");
    let z_view = ctx2.get_instance(&z, &ctx2.human).await?;
    assert!(z_view["fencedAt"].is_null());
    Ok(())
}

// --- 10. Offline running lineage is host-offline, not starting ------------

#[tokio::test]
async fn an_offline_running_lineage_reports_host_offline_and_a_live_one_running() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    let state_of = async {
        let lineage: Value = ctx
            .http
            .get(format!("{}/v1/lineages/{x}", ctx.base()))
            .bearer_auth(&ctx.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(lineage["state"].as_str().unwrap().to_owned())
    };

    // Host link live + chapter running -> running.
    assert_eq!(state_of.await?, "running");

    // Drop the Node: the host loses its link while the chapter is still
    // running. The lineage must NOT read `starting` — the process may be
    // alive (D-019); it is a running chapter on an offline host.
    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;
    let state_of = async {
        let lineage: Value = ctx
            .http
            .get(format!("{}/v1/lineages/{x}", ctx.base()))
            .bearer_auth(&ctx.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(lineage["state"].as_str().unwrap().to_owned())
    };
    assert_eq!(state_of.await?, "host-offline");

    // Process-end evidence on top of the lost link falls back to starting:
    // there is no running chapter anymore and the supervisor has not acted.
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances SET lifecycle = 'exited', activity = 'idle'
             WHERE id = ?1",
            rusqlite::params![x],
        )?;
    }
    let state_of = async {
        let lineage: Value = ctx
            .http
            .get(format!("{}/v1/lineages/{x}", ctx.base()))
            .bearer_auth(&ctx.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(lineage["state"].as_str().unwrap().to_owned())
    };
    assert_eq!(state_of.await?, "starting");
    Ok(())
}

// --- 9. Live chapter without a session is refused, not relaunched blank ---

#[tokio::test]
async fn a_live_chapter_without_a_native_session_is_refused_and_an_ended_one_relaunches()
-> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    // Deliberately NO session report: the chapter is live but never produced a
    // native session id. (seat() already drained the forwarded create frame.)

    let refused = ctx.resume(&x, &ctx.human).await?;
    assert_eq!(refused.status(), 409);
    let body = refused.text().await?;
    assert!(
        body.contains("native session"),
        "the refusal must name the missing session: {body}"
    );
    // No continuation write: still one chapter, generation 1, no fence.
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["generation"], json!(1));
    assert_eq!(lineage["chapters"].as_array().unwrap().len(), 1);
    let x_view = ctx.get_instance(&x, &ctx.human).await?;
    assert!(x_view["fencedAt"].is_null());
    let drained = tokio::time::timeout(Duration::from_millis(150), node.next_frame()).await;
    assert!(drained.is_err(), "a refusal forwards no close/create");

    // Once there is real process-end evidence, the origin-spec fresh launch is
    // the correct recovery and succeeds (r2-9 is about the LIVE case).
    node.appends.send((x.clone(), exited()))?;
    ctx.wait_until(&x, |view| view["lifecycle"] == json!("exited"))
        .await?;
    let recovered: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(recovered["instance"]["generation"], json!(2));
    assert_eq!(recovered["instance"]["chapterCause"], json!("owner-resume"));
    let (method, _) = node.next_frame().await?;
    assert_eq!(
        method, "instance.create",
        "an ended chapter with no session relaunches fresh"
    );
    Ok(())
}

// --- 3. Concurrent resumes: one successor (generation CAS) ---------------

/// Fetch the `endedAt` of a lineage's first chapter.
async fn first_chapter_ended_at(ctx: &Ctx, lineage_id: &str) -> Result<Option<Value>> {
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{lineage_id}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(lineage["chapters"][0]["endedAt"].as_str().map(Value::from))
}

/// r3 item 2: a failed SDK TURN does not end the chapter. `endedAt` stays null
/// through the turn error and is stamped only by the later real exit.
#[tokio::test]
async fn a_failed_turn_sets_no_ended_at_until_the_real_process_exit() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // A settled turn error (the print/sdk mapper's `result` is_error frame):
    // topic=turn, status error, queued 0. The chapter is still running.
    // ma-lineage r6 item 6: capture the durable seq and wait until THIS event
    // is applied (durableSeq != "0" is already true after report_session).
    let seq_before = ctx.get_instance(&x, &ctx.human).await?["durableSeq"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-06T01:00:00.000Z",
            "payload": {
                "type": "native", "topic": "turn", "nativeName": "result",
                "status": { "state": "known", "value": "error" },
                "severity": "info", "affectsCompletion": true,
                "relatedIds": { "queuedTurnCount": "0", "lastError": "API Error: 429" }
            }
        }),
    ))?;
    let view = ctx.wait_seq_advances(&x, &seq_before).await?;
    assert_ne!(
        view["lifecycle"],
        json!("failed"),
        "a turn error never marks the lifecycle failed"
    );
    assert_eq!(
        first_chapter_ended_at(&ctx, &x).await?,
        None,
        "a turn error stamps no endedAt"
    );

    // The real process exits later: only now is endedAt set.
    // The REAL print/SDK exit shape via the shared helper.
    node.appends
        .send((x.clone(), sdk_session_exited("2026-10-06T02:00:00.000Z")))?;
    ctx.wait_until(&x, |v| v["lifecycle"] == json!("exited"))
        .await?;
    assert_eq!(
        first_chapter_ended_at(&ctx, &x).await?,
        Some(json!("2026-10-06T02:00:00.000Z")),
        "endedAt comes from the real SDK/print exit, not the turn error"
    );
    Ok(())
}

/// ma-lineage r4 item 2: a clean PTY exit (native_exit / status exited /
/// severity info) is lifecycle EXITED — not failed — and stamps endedAt; a
/// non-zero exit is failed. Both come from the shared classifier's REAL
/// driver shapes.
#[tokio::test]
async fn real_native_exit_shapes_classify_and_stamp_ended_at() -> Result<()> {
    // Clean PTY exit → exited.
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    node.appends
        .send((x.clone(), pty_native_exit("2026-10-06T03:00:00.000Z", 0)))?;
    let view = ctx
        .wait_until(&x, |v| v["lifecycle"] == json!("exited"))
        .await?;
    assert_eq!(
        view["lifecycle"],
        json!("exited"),
        "a clean native_exit (info) is exited, not failed"
    );
    assert_eq!(
        first_chapter_ended_at(&ctx, &x).await?,
        Some(json!("2026-10-06T03:00:00.000Z"))
    );

    // Non-zero PTY exit → failed.
    let ctx2 = Ctx::boot().await?;
    let mut node2 = FakeNode::connect(&ctx2.hub, &ctx2.host).await?;
    let (z, _token2) = ctx2.seat(&mut node2, None).await?;
    ctx2.report_session(&node2, &z, false).await?;
    node2
        .appends
        .send((z.clone(), pty_native_exit("2026-10-06T03:30:00.000Z", 1)))?;
    let view2 = ctx2
        .wait_until(&z, |v| v["lifecycle"] == json!("failed"))
        .await?;
    assert_eq!(
        view2["lifecycle"],
        json!("failed"),
        "exit 1 is a failed end"
    );
    assert_eq!(
        first_chapter_ended_at(&ctx2, &z).await?,
        Some(json!("2026-10-06T03:30:00.000Z"))
    );
    Ok(())
}

/// r3 item 2: an error-severity session event without exit evidence does not
/// end a live chapter, and a later `ready` entity event leaves endedAt null.
#[tokio::test]
async fn an_error_severity_event_then_ready_leaves_ended_at_null() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    let seq_before = ctx.get_instance(&x, &ctx.human).await?["durableSeq"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-06T01:00:00.000Z",
            "payload": {
                "type": "native", "topic": "session", "nativeName": "transient_runtime_error",
                "status": { "state": "known", "value": "working" },
                "severity": "error", "affectsCompletion": false,
                "relatedIds": {}
            }
        }),
    ))?;
    ctx.wait_seq_advances(&x, &seq_before).await?;
    let view = ctx.get_instance(&x, &ctx.human).await?;
    assert_ne!(
        view["lifecycle"],
        json!("failed"),
        "a transient error is not process end"
    );
    assert_eq!(first_chapter_ended_at(&ctx, &x).await?, None);

    // A later ready entity event confirms the process is alive.
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-06T01:05:00.000Z",
            "payload": { "type": "entity", "state": "ready", "reasonCode": "driver-started" }
        }),
    ))?;
    ctx.wait_until(&x, |v| v["lifecycle"] == json!("running"))
        .await?;
    assert_eq!(
        first_chapter_ended_at(&ctx, &x).await?,
        None,
        "ready on a live chapter leaves endedAt null"
    );
    Ok(())
}

/// r3 item 3: an ATTESTED LAUNCH FAILURE (the launch never started, no native
/// session) is process-end evidence; the owner's resume takes the
/// fresh-recovery path (instance.create), not the live-chapter 409.
#[tokio::test]
async fn a_sessionless_launch_failure_resumes_via_fresh_recovery() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    // Deliberately NO session: the child never initialized. Drain the create.
    // Then the Node attests the launch failed (it rejects before init).
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-06T01:00:00.000Z",
            "payload": {
                "type": "native", "topic": "session",
                "nativeName": "native-driver-start-failed",
                "status": { "state": "known", "value": "failed" },
                "severity": "error", "affectsCompletion": true,
                "relatedIds": {
                    "reasonCode": "native-driver-start-failed",
                    "lastError": "agent process exited during startup"
                }
            }
        }),
    ))?;
    ctx.wait_until(&x, |v| v["lifecycle"] == json!("failed"))
        .await?;
    let failed = ctx.get_instance(&x, &ctx.human).await?;
    assert!(failed["nativeSessionId"].is_null());

    // Resume a sessionless, launch-failed chapter: fresh recovery (200, not
    // 409), and the queued command is a fresh instance.create.
    let recovered: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(recovered["instance"]["generation"], json!(2));
    let (method, params) = node.next_frame().await?;
    assert_eq!(
        method, "instance.create",
        "an attested launch failure relaunches fresh"
    );
    assert!(
        params["spec"]
            .get("resumeSessionId")
            .is_none_or(|v| v.is_null()),
        "fresh recovery must not carry a resume session id"
    );
    Ok(())
}

/// ma-lineage r5 item 4: a sessionless holder the stale-create sweep failed
/// with the Hub's EXACT `create-never-acknowledged` marker recovers fresh on
/// resume (not 409). The marker is matched literally, not by the prose
/// "never acknowledged" string.
#[tokio::test]
async fn a_swept_never_acknowledged_create_with_the_real_marker_recovers_fresh() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    // Simulate the stale-create sweep exactly: failed, no session ever
    // reported, last_error is the constant the sweep stamps. ma-lineage r6
    // item 6: leave ended_at NULL — the marker alone is the evidence (it must
    // not require ended_at to recover).
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    db.execute(
        "UPDATE instances SET lifecycle = 'failed', ended_at = NULL,
            last_error = 'create-never-acknowledged'
         WHERE id = ?1",
        rusqlite::params![x],
    )?;
    drop(db);

    let recovered = ctx.resume(&x, &ctx.human).await?;
    assert_eq!(
        recovered.status(),
        200,
        "the exact create-never-acknowledged marker is launch-failure evidence: fresh recovery"
    );
    let body: Value = recovered.json().await?;
    assert_eq!(body["instance"]["generation"], json!(2));
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.create", "fresh relaunch, not resume");
    Ok(())
}

/// ma-lineage r7 item 3: a continuation successor Y is still `requested` when
/// its host drops before the Node acks. Its durable_seq is already 1 from the
/// Hub-authored `resumed-from` link, so the stale-create sweep used to skip
/// it forever. The host-lost sweep must leave the requested row alone; the
/// stale-create sweep then reaps it (failed / create-never-acknowledged),
/// after which a resume continues the lineage instead of replaying dead Y.
#[tokio::test]
async fn a_requested_successor_with_only_the_hub_resume_link_is_reaped_then_continued() -> Result<()>
{
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let node_token = node_token.context("node token")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // Continue X → Y. The Node never acknowledges Y (no journal from it).
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");

    // Y's durable cursor advanced ONLY because the Hub journaled the
    // resumed-from link; Y is still requested.
    let y_view = ctx.wait_seq_advances(&y, "0").await?;
    assert_eq!(
        y_view["lifecycle"],
        json!("requested"),
        "the successor is requested with a Hub-authored journal event only"
    );

    // Host drops before the Node acks.
    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;
    let (swept, _) = ctx.hub.store().expect("store").expire_lost_hosts(0).await?;
    assert_eq!(
        swept, 1,
        "the host-lost sweep ends the live predecessor X only"
    );
    let (expired, _) = ctx
        .hub
        .store()
        .expect("store")
        .expire_stale_requested(0)
        .await?;
    assert!(
        expired.iter().any(|(_host, id)| id == &y),
        "the requested successor is reaped despite durable_seq > 0: {expired:?}"
    );
    let y_view = ctx.get_instance(&y, &ctx.human).await?;
    assert_eq!(y_view["lifecycle"], json!("failed"));
    assert_eq!(
        y_view["lastError"],
        json!("create-never-acknowledged"),
        "the row keeps its attested launch-failure marker"
    );
    let ended_at: Option<String> = {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.query_row(
            "SELECT ended_at FROM instances WHERE id = ?1",
            rusqlite::params![y],
            |row| row.get(0),
        )?
    };
    assert!(
        ended_at.is_some(),
        "the reaped row carries process-end evidence"
    );

    // Reconnect: a resume addressed to the older chapter X (or the lineage
    // id) must CONTINUE — a new chapter, instance.resume — never replay the
    // dead requested-then-failed Y forever.
    let mut node = FakeNode::connect_with_token(&ctx.hub, &ctx.host, &node_token).await?;
    let response = ctx.resume(&x, &ctx.human).await?.error_for_status()?;
    let third: Value = response.json().await?;
    assert_ne!(third["replayed"], json!(true), "dead Y is not replayed");
    let z = third["instance"]["instanceId"].as_str().unwrap().to_owned();
    assert_ne!(z, y);
    assert_eq!(third["instance"]["generation"], json!(3));
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume", "Z resumes the known session");
    assert_eq!(params["spec"]["resumeSessionId"], json!(SESSION));
    Ok(())
}

/// ma-lineage r7 item 3: once the requested-then-reaped successor is failed
/// with the launch-failure marker, it releases the address-owner seat so a
/// replacement holder can be created.
#[tokio::test]
async fn a_reaped_requested_successor_releases_the_address_owner_seat() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let node_token = node_token.context("node token")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    ctx.wait_seq_advances(&y, "0").await?;

    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;
    ctx.hub.store().expect("store").expire_lost_hosts(0).await?;
    let (expired, _) = ctx
        .hub
        .store()
        .expect("store")
        .expire_stale_requested(0)
        .await?;
    assert!(expired.iter().any(|(_host, id)| id == &y));

    let _node = FakeNode::connect_with_token(&ctx.hub, &ctx.host, &node_token).await?;
    let second = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "grants": ["address-owner"], "prompt": "replacement seat"
        }))
        .send()
        .await?;
    assert_eq!(
        second.status(),
        200,
        "the reaped successor no longer holds the address-owner seat"
    );
    Ok(())
}

/// ma-lineage r7 item 5(c): a nodeEpoch change ends an unreported live holder
/// WITH process-end evidence, so the address-owner seat is released and a
/// replacement holder can be created (HTTP-level proof).
#[tokio::test]
async fn an_epoch_change_end_releases_the_address_owner_seat_end_to_end() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer_hello(
        &ctx.hub,
        &ctx.host,
        &enroll,
        json!({"nodeEpoch": "epoch-1"}),
    )
    .await?;
    let node_token = node_token.context("node token")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;

    // Reconnect under a NEW epoch with an attested empty inventory: the Node
    // holds nothing, so X ends with node-epoch-changed + endedAt.
    let mut node = FakeNode::connect_with_token_hello(
        &ctx.hub,
        &ctx.host,
        &node_token,
        json!({"nodeEpoch": "epoch-2", "instances": [], "instanceStoreFound": true}),
    )
    .await?;
    ctx.wait_until(&x, |view| {
        view["lifecycle"] == json!("exited") && view["lastError"] == json!("node-epoch-changed")
    })
    .await?;

    // The seat is free: a second address-owner holder is accepted and
    // launched.
    let second = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "grants": ["address-owner"], "prompt": "replacement seat"
        }))
        .send()
        .await?;
    assert_eq!(
        second.status(),
        200,
        "the epoch-ended holder released the seat"
    );
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.create");
    Ok(())
}

/// ma-lineage r7 item 5(d): a chapter that is terminal WITHOUT process-end
/// evidence (the host-lost contact-loss marker) derives host-offline while its
/// host link is down, and starting once the link is live again.
#[tokio::test]
async fn an_evidence_less_terminal_chapter_derives_host_offline_then_starting() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) =
        FakeNode::connect_bearer_hello(&ctx.hub, &ctx.host, &enroll, json!({})).await?;
    let node_token = node_token.context("node token")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    node.disconnect().await;
    drop(node);
    ctx.wait_host_offline().await?;
    let (swept, _) = ctx.hub.store().expect("store").expire_lost_hosts(0).await?;
    assert_eq!(swept, 1);

    let lineage_state = || async {
        let lineage: Value = ctx
            .http
            .get(format!("{}/v1/lineages/{x}", ctx.base()))
            .bearer_auth(&ctx.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(lineage["state"].as_str().unwrap().to_owned())
    };
    assert_eq!(
        lineage_state().await?,
        "host-offline",
        "an evidence-less terminal chapter on a down host derives host-offline"
    );

    // Same epoch, same token: no epoch change, link live — the ambiguous
    // chapter reads as starting, not as a genuine running/ended session.
    let _node = FakeNode::connect_with_token(&ctx.hub, &ctx.host, &node_token).await?;
    assert_eq!(lineage_state().await?, "starting");
    Ok(())
}

/// ma-lineage r7 item 5(d): a `{mode:"terminal"}` continuation of a
/// claude-sdk session launches claude-pty and keeps the resume session id —
/// the requested mode's driver is honoured, not the structured default.
#[tokio::test]
async fn a_terminal_mode_continuation_launches_claude_pty_with_the_session() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    let response = ctx.resume_mode(&x, &ctx.human, "terminal").await?;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await?;
    assert_eq!(body["replayed"], json!(false));

    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.close", "the live predecessor is closed");
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(
        params["spec"]["driver"],
        json!("claude-pty"),
        "terminal mode maps the successor to claude-pty"
    );
    assert_eq!(
        params["spec"]["resumeSessionId"],
        json!(SESSION),
        "the native session is resumed, not relaunched blank"
    );
    Ok(())
}

/// ma-lineage r7 item 6(a): a `{mode:"terminal"}` continuation that takes the
/// FRESH path (the chapter ended without ever reporting a native session)
/// launches claude-pty — both in the queued create spec and on the successor
/// ROW — not the predecessor's claude-sdk driver.
#[tokio::test]
async fn a_terminal_mode_fresh_recovery_launches_claude_pty() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    // Deliberately report NO session: just a real process-end entity event.
    node.appends.send((x.clone(), exited()))?;
    ctx.wait_until(&x, |view| view["lifecycle"] == json!("exited"))
        .await?;

    let response = ctx.resume_mode(&x, &ctx.human, "terminal").await?;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await?;
    assert_eq!(
        body["instance"]["driver"],
        json!("claude-pty"),
        "the successor row is stamped with the mode-mapped driver immediately"
    );

    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.create", "no session means a fresh launch");
    assert_eq!(params["spec"]["driver"], json!("claude-pty"));
    assert!(
        params["spec"]
            .get("resumeSessionId")
            .is_none_or(|v| v.is_null()),
        "fresh recovery carries no resume session id"
    );
    Ok(())
}

/// r3 item 4: an immediate retry of a sessionless fresh recovery returns the
/// SAME successor (replayed) before the successor reports a session — no new
/// generation and no extra command.
#[tokio::test]
async fn a_retried_fresh_recovery_replays_the_same_successor_before_it_reports_a_session()
-> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    node.appends.send((
        x.clone(),
        json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-06T01:00:00.000Z",
            "payload": {
                "type": "native", "topic": "session",
                "nativeName": "native-driver-start-failed",
                "status": { "state": "known", "value": "failed" },
                "severity": "error", "affectsCompletion": true,
                "relatedIds": { "reasonCode": "native-driver-start-failed" }
            }
        }),
    ))?;
    ctx.wait_until(&x, |v| v["lifecycle"] == json!("failed"))
        .await?;

    // First resume: fresh recovery X (gen 1) -> Y (gen 2), one create.
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(first["replayed"], json!(false));
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.create");

    // Immediate retry addressed to X, before Y reports any session.
    let second: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        second["replayed"],
        json!(true),
        "retry is an idempotent replay"
    );
    assert_eq!(
        second["instance"]["instanceId"],
        json!(y),
        "the retry returns the same successor"
    );

    // No third generation, no second forwarded command.
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["generation"], json!(2));
    assert_eq!(lineage["chapters"].as_array().unwrap().len(), 2);
    let extra = tokio::time::timeout(Duration::from_millis(150), node.next_frame()).await;
    assert!(extra.is_err(), "the replay forwards no extra command");
    Ok(())
}

/// A resume addressed to an already-fenced chapter returns its existing
/// successor idempotently (r2-8): it must not fence the live chapter or mint
/// another generation.
#[tokio::test]
async fn a_resume_addressed_to_a_fenced_chapter_returns_its_successor() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // First continuation: X (gen 1) -> Y (gen 2).
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(first["replayed"], json!(false));
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (_close, _) = node.next_frame().await?;
    let (_method, _) = node.next_frame().await?;
    ctx.report_session(&node, &y, false).await?;

    // Second resume, still addressed to the now-fenced X.
    let second: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        second["replayed"],
        json!(true),
        "a fenced chapter resumes as an idempotent replay of its successor"
    );
    assert_eq!(
        second["instance"]["instanceId"],
        json!(y),
        "the existing successor is returned, not a new chapter"
    );

    // No third chapter, no extra fencing: generation stays 2 and no frame
    // targets Y (the live chapter is never closed by the stale address).
    let lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(lineage["generation"], json!(2));
    assert_eq!(lineage["chapters"].as_array().unwrap().len(), 2);
    let stale = tokio::time::timeout(Duration::from_millis(200), node.next_frame()).await;
    assert!(
        stale.is_err(),
        "a replay addressed to a fenced chapter forwards nothing"
    );

    // Resuming the CURRENT chapter still works and advances to generation 3.
    let third: Value = ctx
        .resume(&y, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(third["replayed"], json!(false));
    assert_ne!(third["instance"]["instanceId"], json!(y));
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.close");
    assert_eq!(
        params["instanceId"],
        json!(y),
        "only the live chapter is fenced"
    );
    Ok(())
}

#[tokio::test]
async fn two_concurrent_resumes_produce_one_successor() -> Result<()> {
    // The Hub has one FIFO SQLite writer thread: two resume jobs queue in
    // order. Authentication also takes a writer slot, so two HTTP requests
    // cannot both pass their generation read before the first fence job
    // commits; the deterministic race is therefore driven at the store, which
    // is the boundary the CAS actually protects.
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, true).await?;
    let store = ctx.hub.store().context("hub store")?.clone();

    // The first CAS entry FOR THIS LINEAGE blocks on the gate; every later
    // entry (the queued second job) passes immediately. The hook is
    // process-global and other lineage tests run in parallel in this binary,
    // so it must ignore every other lineage id — otherwise an unrelated
    // resume could park on this test's gate. `entries` lets the test know the
    // first fence transaction is open before it enqueues the loser.
    let entries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let hook_entries = entries.clone();
    let target_lineage = x.clone();
    let hook: remuda_hub::lineage_test_support::Hook =
        std::sync::Arc::new(move |point: &str, lineage: &str| {
            if point != "cas" || lineage != target_lineage {
                return;
            }
            if hook_entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let guard = go_rx.lock().unwrap();
                loop {
                    match guard.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(err) => panic!("gate closed: {err}"),
                    }
                }
            }
        });
    remuda_hub::lineage_test_support::set_hook(Some(hook));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            remuda_hub::lineage_test_support::set_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    // The first job reaches the CAS and parks.
    let store1 = store.clone();
    let (x1, h1) = (x.clone(), ctx.host.clone());
    let first = tokio::spawn(async move {
        let req = remuda_hub::store_test_support::ContinuationResumeRequest {
            addressed_instance_id: x1.clone(),
            expected_generation: 1,
            host_id: h1,
            spec: json!({
                "kind": "claude", "driver": "claude-sdk",
                "resumeSessionId": SESSION, "resumedFrom": x1
            }),
            operation: "instance.resume".to_owned(),
            prompt: None,
            origin: "human".to_owned(),
            title: None,
            driver: "claude-sdk".to_owned(),
        };
        store1.continuation_resume(req).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while entries.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;

    // The second job is enqueued behind the open fence transaction (the
    // mpsc send in `run_named` completes while the winner parks), then loses
    // the generation CAS.
    let store2 = store.clone();
    let (x2, h2) = (x.clone(), ctx.host.clone());
    let second = tokio::spawn(async move {
        let req = remuda_hub::store_test_support::ContinuationResumeRequest {
            addressed_instance_id: x2.clone(),
            expected_generation: 1,
            host_id: h2,
            spec: json!({
                "kind": "claude", "driver": "claude-sdk",
                "resumeSessionId": SESSION, "resumedFrom": x2
            }),
            operation: "instance.resume".to_owned(),
            prompt: None,
            origin: "human".to_owned(),
            title: None,
            driver: "claude-sdk".to_owned(),
        };
        store2.continuation_resume(req).await
    });
    // Let the loser's job land in the writer queue before opening the gate.
    tokio::time::sleep(Duration::from_millis(50)).await;
    go_tx.send(())?;

    let winner = first.await??;
    let loser = second.await??;
    let (winner_id, loser_id) = match (&winner, &loser) {
        (
            remuda_hub::store_test_support::ContinuationResumeResult::Resumed(resumed),
            remuda_hub::store_test_support::ContinuationResumeResult::Superseded { current },
        ) => (
            resumed.successor.instance_id.clone(),
            current.instance_id.clone(),
        ),
        other => panic!("expected Resumed + Superseded, got {other:?}"),
    };
    assert_eq!(
        winner_id, loser_id,
        "the loser returns the winner's successor"
    );

    // Exactly one successor: generation 2, two chapters.
    let lineage = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(lineage["generation"], json!(2));
    assert_eq!(lineage["chapters"].as_array().unwrap().len(), 2);
    assert_eq!(
        entries.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "both transactions reached the CAS hook"
    );
    Ok(())
}

// --- 4. Ownership across chapters ----------------------------------------

#[tokio::test]
async fn successor_owns_sends_stops_and_reads_what_the_predecessor_created() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, x_token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    // X creates a worker before the resume.
    let worker = ctx.agent_child(&x_token, &mut node).await?;

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let _y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    // Drain the predecessor close; capture the successor launch token.
    let (close_method, _) = node.next_frame().await?;
    assert_eq!(close_method, "instance.close");
    let (_method, resume_params) = node.next_frame().await?;
    let y_token = resume_params["agentCredential"]["token"]
        .as_str()
        .context("successor token")?
        .to_owned();
    assert_eq!(
        response["instance"]["lineageId"],
        json!(x),
        "sanity: successor is chapter 2 of the predecessor's lineage"
    );

    // Y reads X's journal across the lineage.
    let journal = ctx
        .http
        .get(format!("{}/v1/instances/{x}/journal", ctx.base()))
        .bearer_auth(&y_token)
        .send()
        .await?;
    assert_eq!(journal.status(), 200);

    // Y sends to the worker X created.
    let send = ctx
        .http
        .post(format!("{}/v1/instances/{worker}/commands", ctx.base()))
        .bearer_auth(&y_token)
        .json(&json!({
            "operation": "instance.send",
            "payload": { "input": { "type": "prompt", "text": "from successor" } }
        }))
        .send()
        .await?;
    assert_eq!(send.status(), 200, "{}", send.text().await?);

    // Y stops the worker.
    let stop = ctx
        .http
        .post(format!("{}/v1/instances/{worker}/commands", ctx.base()))
        .bearer_auth(&y_token)
        .json(&json!({ "operation": "instance.close", "payload": {} }))
        .send()
        .await?;
    assert_eq!(stop.status(), 200, "{}", stop.text().await?);

    let mut observed = Vec::new();
    while let Ok(Some(frame)) =
        tokio::time::timeout(Duration::from_millis(300), node.frames.recv()).await
    {
        let (method, params) = frame;
        observed.push((method, params));
    }
    let saw_send = observed
        .iter()
        .any(|(method, params)| method == "instance.send" && params["instanceId"] == json!(worker));
    let saw_stop = observed.iter().any(|(method, params)| {
        method == "instance.close" && params["instanceId"] == json!(worker)
    });
    assert!(saw_send, "successor sent to the predecessor's worker");
    assert!(saw_stop, "successor stopped the predecessor's worker");

    // A foreign human-seated instance stays outside the lineage: no read.
    let foreign: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty", "prompt": "foreign"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let foreign_id = foreign["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = node.next_frame().await?;
    let denied = ctx
        .http
        .get(format!("{}/v1/instances/{foreign_id}", ctx.base()))
        .bearer_auth(&y_token)
        .send()
        .await?;
    assert_eq!(denied.status(), 403);

    Ok(())
}

// --- 5. D-051 edge across chapters ---------------------------------------

#[tokio::test]
async fn delegated_question_created_under_a_predecessor_reaches_the_successor() -> Result<()> {
    let _flag = d051_on().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, x_token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;
    // A non-bypass child of X asks a question before the resume.
    let worker = ctx.agent_child(&x_token, &mut node).await?;
    let interaction_id = remuda_protocol::InteractionId::new()
        .as_id()
        .as_str()
        .to_string();
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "INSERT INTO interactions
                (id, instance_id, host_id, kind, state, blocking, payload_json,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, 'question', 'pending', 1, '{}', ?4, ?4)",
            rusqlite::params![interaction_id, worker, ctx.host, "2026-10-05T00:00:00.000Z"],
        )?;
    }

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let (_close, _) = node.next_frame().await?;
    let (_method, resume_params) = node.next_frame().await?;
    let y_token = resume_params["agentCredential"]["token"]
        .as_str()
        .context("successor token")?
        .to_owned();
    let _y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();

    // The successor lists the predecessor's child's question.
    let list: Value = ctx
        .http
        .get(format!("{}/v1/interactions", ctx.base()))
        .bearer_auth(&y_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ids: Vec<String> = list["items"]
        .as_array()
        .context("items")?
        .iter()
        .filter_map(|item| item["interactionId"].as_str().map(str::to_string))
        .collect();
    assert!(
        ids.contains(&interaction_id),
        "question must list for successor"
    );

    // ...and can answer it.
    let answer = ctx
        .http
        .post(format!(
            "{}/v1/interactions/{interaction_id}/answer",
            ctx.base()
        ))
        .bearer_auth(&y_token)
        .json(&json!({ "answer": { "kind": "question", "answers": {} } }))
        .send()
        .await?;
    assert_eq!(answer.status(), 200, "{}", answer.text().await?);
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    let state: String = db.query_row(
        "SELECT state FROM interactions WHERE id = ?1",
        rusqlite::params![interaction_id],
        |row| row.get(0),
    )?;
    assert_eq!(state, "answer-committed");

    Ok(())
}

/// D-051 (6d) across chapters: a plan review is the lineage's OWN plan, so no
/// chapter of that lineage can review it — including the successor chapter,
/// which the old instance-id self comparison let through. The direct parent
/// (outside the lineage) and the human still review it.
#[tokio::test]
async fn a_successor_chapter_cannot_review_its_predecessors_plan() -> Result<()> {
    let _flag = d051_on().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let (x, x_token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // A plan review owned by the seat X (predecessor chapter), pending before
    // the continuation.
    let plan_id = remuda_protocol::InteractionId::new()
        .as_id()
        .as_str()
        .to_string();
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "INSERT INTO interactions
                (id, instance_id, host_id, kind, state, blocking, payload_json,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, 'plan-review', 'pending', 1, '{}', ?4, ?4)",
            rusqlite::params![plan_id, x, ctx.host, "2026-10-05T00:00:00.000Z"],
        )?;
    }

    // The predecessor chapter cannot review its own plan (same lineage, same
    // rule) — checked BEFORE the continuation, while its credential is live.
    // The edge is lineage membership, not instance identity.
    let predecessor_self = ctx
        .http
        .post(format!("{}/v1/interactions/{plan_id}/answer", ctx.base()))
        .bearer_auth(&x_token)
        .json(&json!({ "answer": {
            "kind": "plan-review", "optionId": "approve",
            "planRevision": "1",
            "planDigest": format!("sha256:{}", "a".repeat(64)),
            "feedback": null
        } }))
        .send()
        .await?;
    assert_eq!(predecessor_self.status(), 403);

    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let (_close, _) = node.next_frame().await?;
    let (_method, resume_params) = node.next_frame().await?;
    let y_token = resume_params["agentCredential"]["token"]
        .as_str()
        .context("successor token")?
        .to_owned();
    let y = response["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();

    // The successor chapter does not see the plan in its routed list...
    let list: Value = ctx
        .http
        .get(format!("{}/v1/interactions", ctx.base()))
        .bearer_auth(&y_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ids: Vec<String> = list["items"]
        .as_array()
        .context("items")?
        .iter()
        .filter_map(|item| item["interactionId"].as_str().map(str::to_string))
        .collect();
    assert!(
        !ids.contains(&plan_id),
        "a successor chapter must not receive its predecessor lineage's own plan"
    );

    // ...and answering it directly is forbidden (unknown-id-shaped 403, not
    // 404: the interaction exists, the lineage may just not review it).
    let answer = ctx
        .http
        .post(format!("{}/v1/interactions/{plan_id}/answer", ctx.base()))
        .bearer_auth(&y_token)
        .json(&json!({ "answer": {
            "kind": "plan-review", "optionId": "approve",
            "planRevision": "1",
            "planDigest": format!("sha256:{}", "a".repeat(64)),
            "feedback": null
        } }))
        .send()
        .await?;
    assert_eq!(answer.status(), 403);
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    let state: String = db.query_row(
        "SELECT state FROM interactions WHERE id = ?1",
        rusqlite::params![plan_id],
        |row| row.get(0),
    )?;
    assert_eq!(state, "pending", "the rejected answer must not commit");

    // The predecessor chapter itself could not self-review either (same
    // lineage, same rule) — the edge is membership, not instance identity.
    // (After fencing its launch credential is revoked, which is 401; the
    // rule itself is asserted above before the continuation.)

    // The human still reviews the plan (not an Agent lineage member).
    let human_answer = ctx
        .http
        .post(format!("{}/v1/interactions/{plan_id}/answer", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({ "answer": {
            "kind": "plan-review", "optionId": "approve",
            "planRevision": "1",
            "planDigest": format!("sha256:{}", "a".repeat(64)),
            "feedback": null
        } }))
        .send()
        .await?;
    assert_eq!(human_answer.status(), 200, "{}", human_answer.text().await?);
    let _ = y;
    Ok(())
}

// --- 6. Fan-out across chapters and equal depth --------------------------

/// Drain forwarded frames until `instanceId == id` and the method matches.
async fn drain_frame(node: &mut FakeNode, method: &str, id: &str) -> Result<Value> {
    for _ in 0..8 {
        let (got_method, params) = node.next_frame().await?;
        if got_method == method && params["instanceId"] == json!(id) {
            return Ok(params);
        }
    }
    anyhow::bail!("never saw {method} for {id}")
}

#[tokio::test]
async fn fan_out_counts_across_chapters_and_continuation_adds_no_depth() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;

    // Project policy: fan-out 2, depth 2 (chapter at depth 1, child at 2).
    let project: Value = ctx
        .http
        .post(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "name": "fanout",
            "policy": { "configurable": { "coordinatorFanOut": 2, "maxDelegationDepth": 2 } }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let project_id = project["id"].as_str().context("project id")?.to_owned();
    let scope = json!({ "projectIds": [project_id] });
    let (x, x_token) = ctx.seat(&mut node, Some(scope)).await?;
    ctx.report_session(&node, &x, false).await?;

    // Two active children under the first chapter.
    let w1 = ctx.agent_child(&x_token, &mut node).await?;
    let w2 = ctx.agent_child(&x_token, &mut node).await?;

    let _resumed: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let (_close, _) = node.next_frame().await?;
    let (_method, resume_params) = node.next_frame().await?;
    let y_token = resume_params["agentCredential"]["token"]
        .as_str()
        .unwrap()
        .to_owned();

    // A third child from the successor hits the fan-out limit that counts X's
    // children — NOT the depth limit, proving continuation added no depth.
    let refused = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&y_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual", "prompt": "third"
        }))
        .send()
        .await?;
    assert_eq!(refused.status(), 409);
    let message = refused.text().await?;
    assert!(
        message.contains("fan-out"),
        "expected fan-out refusal: {message}"
    );

    // After one child ends, the successor can delegate again: the chapters
    // share one budget.
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances SET lifecycle = 'closed' WHERE id = ?1",
            rusqlite::params![w1],
        )?;
    }
    let w3 = ctx.agent_child(&y_token, &mut node).await?;
    assert_ne!(w3, w2);
    Ok(())
}

/// A child lineage that continued — its first chapter ended and its successor
/// chapter is the live one — still occupies one fan-out slot. The pre-round-2
/// per-row count dropped continuation rows (`chapter_cause IS NULL`), so a
/// resumed worker silently stopped counting.
#[tokio::test]
async fn a_resumed_child_lineage_still_counts_against_fan_out() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;

    let project: Value = ctx
        .http
        .post(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "name": "fanout-resumed-child",
            "policy": { "configurable": { "coordinatorFanOut": 2, "maxDelegationDepth": 3 } }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let project_id = project["id"].as_str().context("project id")?.to_owned();
    // Seat carrying `land` as well: the child needs a grant that makes it a
    // continuity lineage without colliding with the seat's per-project
    // `dispatch` or the hub-wide `address-owner` uniqueness.
    let mut seat_body = json!({
        "hostId": ctx.host,
        "kind": "claude",
        "driver": "claude-sdk",
        "name": "main",
        "title": "Main",
        "grants": ["address-owner", "dispatch", "land"],
        "permissionMode": "manual",
        "restart": Ctx::restart_on(),
        "prompt": "seat brief",
        "scope": { "projectIds": [project_id] },
    });
    let _ = &mut seat_body;
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&seat_body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let x = created["instance"]["instanceId"]
        .as_str()
        .context("seat id")?
        .to_owned();
    let (_method, create_params) = node.next_frame().await?;
    assert_eq!(_method, "instance.create");
    let x_token = create_params["agentCredential"]["token"]
        .as_str()
        .context("launch token")?
        .to_owned();
    ctx.report_session(&node, &x, false).await?;

    // W1 is a continuity worker (holds land, so it has a lineage and can
    // continue); W2 is a plain worker.
    let w1: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&x_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual",
            "grants": ["land"],
            "scope": { "projectIds": [project_id] },
            "prompt": "continuity worker"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let w1 = w1["instance"]["instanceId"].as_str().unwrap().to_owned();
    let _ = node.next_frame().await?;
    let w2 = ctx.agent_child(&x_token, &mut node).await?;
    ctx.report_session(&node, &w1, false).await?;

    // Continue the W1 lineage: W1 ends, W1B is the live successor chapter.
    let resumed: Value = ctx
        .resume(&w1, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let w1b = resumed["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(w1b, w1);
    drain_frame(&mut node, "instance.close", &w1).await?;
    drain_frame(&mut node, "instance.resume", &w1b).await?;
    // W1 genuinely ends (process-end evidence); only W1B stays live.
    node.appends.send((w1.clone(), exited()))?;
    ctx.wait_until(&w1, |view| view["lifecycle"] == json!("exited"))
        .await?;
    ctx.report_session(&node, &w1b, false).await?;

    // Continue the seat lineage as well; fan-out is checked from the
    // successor chapter Y against the SAME shared lineage resolver.
    let _: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    drain_frame(&mut node, "instance.close", &x).await?;
    let y_params = loop {
        let (method, params) = node.next_frame().await?;
        if method == "instance.resume" {
            break params;
        }
    };
    let y_token = y_params["agentCredential"]["token"]
        .as_str()
        .unwrap()
        .to_owned();

    // Resolved through the Agent dispatch path: W1B + W2 fill the budget of 2,
    // so the successor chapter cannot delegate a third worker.
    let refused = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&y_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual", "prompt": "third"
        }))
        .send()
        .await?;
    assert_eq!(refused.status(), 409);
    assert!(
        refused.text().await?.contains("fan-out"),
        "the resumed child lineage must still occupy a fan-out slot"
    );

    // Ending the successor chapter releases the W1 lineage slot; a third
    // dispatch then fits (W2 is the only active child).
    {
        let db = rusqlite::Connection::open(&ctx.db_path)?;
        db.execute(
            "UPDATE instances SET lifecycle = 'closed' WHERE id = ?1",
            rusqlite::params![w1b],
        )?;
    }
    let w3 = ctx.agent_child(&y_token, &mut node).await?;
    assert_ne!(w3, w2);
    Ok(())
}

// --- 7. claude-sdk resume stays claude-sdk and accepts a second turn ------

#[tokio::test]
async fn claude_sdk_parent_resumes_as_claude_sdk_with_resume_session_id() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    // A plain sdk session (no grants, no restart): the carrier fix reaches
    // every claude-sdk session through the D-026 path.
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-sdk", "prompt": "hi"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let parent = created["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_method, _params) = node.next_frame().await?;
    ctx.report_session(&node, &parent, true).await?;

    let resumed: Value = ctx
        .resume(&parent, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(resumed["instance"]["driver"], json!("claude-sdk"));
    assert_eq!(resumed["instance"]["resumedFrom"], json!(parent));
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["driver"], json!("claude-sdk"));
    assert_eq!(params["spec"]["resumeSessionId"], json!(SESSION));

    // The harness accepts a second turn on the resumed sdk chapter.
    let child = resumed["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let send = ctx
        .http
        .post(format!("{}/v1/instances/{child}/commands", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "operation": "instance.send",
            "payload": { "input": { "type": "prompt", "text": "again" } }
        }))
        .send()
        .await?;
    assert_eq!(send.status(), 200);
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.send");
    assert_eq!(params["instanceId"], json!(child));
    Ok(())
}

// --- 8. Plain session resume is byte-identical to D-026 -------------------

#[tokio::test]
async fn plain_non_sdk_session_keeps_todays_d026_resume_semantics() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "prompt": "hi", "tui": "default"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let parent = created["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_method, _params) = node.next_frame().await?;
    ctx.report_session(&node, &parent, true).await?;

    // A plain instance has no lineage row.
    let no_lineage = ctx
        .http
        .get(format!("{}/v1/lineages/{parent}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(no_lineage.status(), 404);

    let resumed: Value = ctx
        .resume(&parent, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let child = resumed["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(child, parent);
    assert_eq!(resumed["instance"]["driver"], json!("claude-print"));
    // D-026 edge: the resume child hangs off the parent, no continuation.
    assert_eq!(resumed["instance"]["parentInstanceId"], json!(parent));
    assert_eq!(resumed["instance"]["resumedFrom"], json!(parent));
    assert!(resumed["instance"]["chapterCause"].is_null());
    assert_eq!(resumed["instance"]["generation"], json!(1));
    assert_eq!(
        resumed["instance"]["lineageId"],
        json!(child),
        "plain child is its own lineage"
    );
    assert!(
        resumed["instance"]["grants"]
            .as_array()
            .is_none_or(|grants| grants.is_empty())
    );

    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["driver"], json!("claude-print"));
    assert_eq!(params["spec"]["resumeSessionId"], json!(SESSION));
    assert_eq!(params["spec"]["parentInstanceId"], json!(parent));
    Ok(())
}

// --- 9. restart is Human-only at create -----------------------------------

#[tokio::test]
async fn restart_policy_from_agent_or_bot_origin_is_forbidden() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host).await?;

    // Agent origin.
    let created: Value = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty", "prompt": "token maker"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let maker = created["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_method, params) = node.next_frame().await?;
    let agent_token = params["agentCredential"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let agent_denied = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&agent_token)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "permissionMode": "manual",
            "restart": { "onProcessLoss": true, "maxPerHour": 3 },
            "prompt": "no restart for agents"
        }))
        .send()
        .await?;
    assert_eq!(agent_denied.status(), 403);

    // Bot origin.
    let bot = ctx.hub.mint_bot_device_token("lineage-bot").await?;
    let bot_denied = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&bot)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-print",
            "restart": { "onProcessLoss": true, "maxPerHour": 3 },
            "prompt": "no restart for bots"
        }))
        .send()
        .await?;
    assert_eq!(bot_denied.status(), 403);

    // Bound validation: maxPerHour must be at least 1.
    let bad = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty",
            "restart": { "onProcessLoss": true, "maxPerHour": 0 },
            "prompt": "bad cap"
        }))
        .send()
        .await?;
    assert_eq!(bad.status(), 400);
    let _ = maker;
    Ok(())
}

// --- 10. Pre-ma-lineage grant holders are backfilled on open ---------------

/// r3 item 1: a reopened database with a continuation (root X + successor Y
/// carrying copied grants) must backfill exactly ONE lineage, keyed by the
/// stamped lineage id X — never a phantom lineage for the successor Y.
#[tokio::test]
async fn reopening_a_continued_lineage_backfills_no_phantom_successor_lineage() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let node_token = node_token.context("nodeToken")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // Continue X -> Y.
    let resumed: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = resumed["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_close, _) = node.next_frame().await?;
    let (_method, _) = node.next_frame().await?;
    assert_eq!(resumed["instance"]["lineageId"], json!(x));
    ctx.report_session(&node, &y, false).await?;

    let data_dir = ctx._dir.path().to_owned();
    let (human, host) = (ctx.human.clone(), ctx.host.clone());
    let Ctx {
        _dir,
        hub,
        http,
        human: _h,
        host: _hh,
        db_path,
    } = ctx;
    hub.shutdown().await;

    // Reopen twice: the migration must never add a lineage keyed by Y.
    for _ in 0..2 {
        let hub = spawn(HubConfig::for_test(data_dir.join("data"))).await?;
        let (total, for_x, for_y): (i64, i64, i64) = {
            let db = rusqlite::Connection::open(&db_path)?;
            (
                db.query_row("SELECT COUNT(*) FROM lineages", [], |row| row.get(0))?,
                db.query_row(
                    "SELECT COUNT(*) FROM lineages WHERE lineage_id = ?1",
                    rusqlite::params![x],
                    |row| row.get(0),
                )?,
                db.query_row(
                    "SELECT COUNT(*) FROM lineages WHERE lineage_id = ?1",
                    rusqlite::params![y],
                    |row| row.get(0),
                )?,
            )
        };
        assert_eq!(
            (total, for_x, for_y),
            (1, 1, 0),
            "no phantom successor lineage"
        );
        hub.shutdown().await;
    }

    // And over HTTP: Y is a chapter, not a lineage root.
    let hub = spawn(HubConfig::for_test(data_dir.join("data"))).await?;
    let node = FakeNode::connect_with_token(&hub, &host, &node_token).await?;
    let ctx = Ctx {
        _dir,
        hub,
        http,
        human,
        host,
        db_path,
    };
    let y_lineage = ctx
        .http
        .get(format!("{}/v1/lineages/{y}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    assert_eq!(
        y_lineage.status(),
        404,
        "a successor chapter is not its own lineage root"
    );
    let x_lineage: Value = ctx
        .http
        .get(format!("{}/v1/lineages/{x}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(x_lineage["generation"], json!(2));
    assert_eq!(x_lineage["chapters"].as_array().unwrap().len(), 2);
    drop(node);
    Ok(())
}

#[tokio::test]
async fn an_existing_grant_holder_gets_a_backfilled_lineage_and_then_resumes_as_a_continuation()
-> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut node, node_token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let node_token = node_token.context("first enrollment mints a nodeToken")?;
    let (x, _token) = ctx.seat(&mut node, None).await?;
    ctx.report_session(&node, &x, false).await?;

    // Simulate a database from the build before ma-lineage: the migration adds
    // the columns, but the holder has no lineage row and no lineage_id stamp.
    let data_dir = ctx._dir.path().to_owned();
    let (human, host) = (ctx.human.clone(), ctx.host.clone());
    let Ctx {
        _dir,
        hub,
        http,
        human: _human,
        host: _host,
        db_path,
    } = ctx;
    hub.shutdown().await;
    {
        let db = rusqlite::Connection::open(&db_path)?;
        db.execute("DELETE FROM lineages", [])?;
        db.execute(
            "UPDATE instances SET lineage_id = NULL WHERE id = ?1",
            rusqlite::params![x],
        )?;
    }

    // Reopen: the migration must backfill exactly one lineage row.
    let hub = spawn(HubConfig::for_test(data_dir.join("data"))).await?;
    macro_rules! assert_backfilled {
        () => {{
            let db = rusqlite::Connection::open(&db_path)?;
            let (count, current, generation, restart): (i64, String, i64, String) = db.query_row(
                "SELECT COUNT(*), COALESCE(current_instance_id,''), COALESCE(generation,0),
                        COALESCE(restart_json,'')
                 FROM lineages WHERE lineage_id = ?1",
                rusqlite::params![x],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(count, 1, "backfill must create exactly one row");
            assert_eq!(current, x);
            assert_eq!(generation, 1);
            assert!(
                restart.contains("onProcessLoss"),
                "restart policy is copied"
            );
        }};
    }
    assert_backfilled!();
    hub.shutdown().await;

    // A second open runs the migration again without duplicating the row.
    let hub = spawn(HubConfig::for_test(data_dir.join("data"))).await?;
    assert_backfilled!();

    // Drive the resumed continuation over HTTP against the reopened Hub: the
    // backfilled holder must resume through the lineage path, not plain D-026.
    // Re-enrollment presents the original host's nodeToken (D-018).
    let mut node = FakeNode::connect_with_token(&hub, &host, &node_token).await?;
    let ctx = Ctx {
        _dir,
        hub,
        http,
        human,
        host,
        db_path,
    };
    let response: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = response["instance"]["instanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    assert_ne!(x, y);
    assert_eq!(response["instance"]["lineageId"], json!(x));
    assert_eq!(response["instance"]["generation"], json!(2));
    assert_eq!(
        response["instance"]["chapterCause"],
        json!("owner-resume"),
        "the backfilled holder resumes as a continuation, not plain D-026"
    );
    assert_eq!(
        response["instance"]["grants"],
        json!(["address-owner", "dispatch"]),
        "the backfilled lineage keeps the holder's grants on the successor"
    );
    let (method, _) = node.next_frame().await?;
    assert_eq!(method, "instance.close", "the live predecessor is closed");
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["resumeSessionId"], json!(SESSION));
    Ok(())
}

/// A Node whose `instance.purge` RPC replies stay parked until the test opens
/// the gate — the deterministic interleave point for a continuation arriving
/// while a whole-lineage delete is purging (r8 item 2).
struct GatedNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    release: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GatedNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl GatedNode {
    async fn connect(hub: &remuda_hub::RunningHub, host: &str, token: &str) -> Result<Self> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        node.send(Message::Text(
            json!({
                "jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params":{"hostId":host,"host":{
                    "hostname":"lineage-gated","workspaces":[],"workspaceRevision":0,
                    "herdr":{"path":"/usr/bin/herdr"},
                    "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged_in"}]
                }}
            })
            .to_string()
            .into(),
        ))
        .await?;
        let hello_text = node.next().await.context("hello")??.into_text()?;
        let hello: Value = serde_json::from_str(&hello_text)?;
        assert!(hello.get("result").is_some(), "hello failed: {hello}");

        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, mut released) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let frame = node.next();
                tokio::select! {
                    frame = frame => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                                continue;
                            };
                            let Some(method) = frame["method"].as_str() else {
                                continue;
                            };
                            if frame_tx.send((method.to_owned(), frame["params"].clone())).is_err() {
                                break;
                            }
                            // Park purge replies until the test opens the gate.
                            if method == "instance.purge" {
                                let _ = released.wait_for(|open| *open).await;
                            }
                            if node.send(Message::Text(json!({
                                "jsonrpc":"2.0","id":frame["id"],
                                "result":{"purged":true}
                            }).to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(_)) | Some(Err(_)) => {}
                    },
                }
            }
        });
        Ok(Self {
            frames: frame_rx,
            release,
            task,
        })
    }

    async fn next_frame(&mut self) -> Result<(String, Value)> {
        tokio::time::timeout(Duration::from_secs(5), self.frames.recv())
            .await?
            .context("frame")
    }
}

/// r8 item 2: a continuation resume landing WHILE the lineage delete is in
/// its purge loop can no longer race the store delete. The Hub rows are
/// removed (re-check included) BEFORE the first purge RPC, so the resume sees
/// a gone lineage (404) and the delete completes 200 — no 409-after-purge,
/// no false/duplicate audit.
#[tokio::test]
async fn resume_during_the_purge_loop_finds_the_lineage_already_gone() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let enroll = ctx.hub.mint_enroll_token(5).await?;
    let (mut setup, _token) = FakeNode::connect_bearer(&ctx.hub, &ctx.host, &enroll).await?;
    let (x, _) = ctx.seat(&mut setup, None).await?;
    // X terminal, then continue to Y; bring Y to exited as well, so the
    // whole-lineage delete is a no-force, no-stop delete of two terminal
    // chapters and the first side effect is the X purge.
    ctx.report_session(&setup, &x, true).await?;
    let first: Value = ctx
        .resume(&x, &ctx.human)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let y = first["instance"]["instanceId"].as_str().unwrap().to_owned();
    let (method, _) = setup.next_frame().await?;
    assert_eq!(method, "instance.resume");
    ctx.report_session(&setup, &y, true).await?;
    setup.disconnect().await;

    let mut node = GatedNode::connect(&ctx.hub, &ctx.host, &_token.expect("node token")).await?;

    let delete = {
        let http = ctx.http.clone();
        let base = ctx.base();
        let human = ctx.human.clone();
        let y = y.clone();
        tokio::spawn(async move {
            http.delete(format!("{base}/v1/instances/{y}"))
                .bearer_auth(human)
                .send()
                .await
        })
    };

    // The delete reaches the purge loop ONLY after the store delete
    // committed (r8 item 2). Wait until the first purge is parked.
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.purge");
    let purged_first = params["instanceId"].as_str().unwrap().to_owned();
    assert!(
        purged_first == x || purged_first == y,
        "unexpected purge target {purged_first}"
    );

    // A continuation of Y landing now finds the lineage gone.
    let raced = ctx.resume(&y, &ctx.human).await?;
    assert_eq!(
        raced.status(),
        404,
        "the lineage rows are already deleted; a mid-purge resume cannot append a successor"
    );

    node.release.send(true).ok();
    let deleted = delete.await??.error_for_status()?;
    let body: Value = deleted.json().await?;
    assert_eq!(body["deleted"], json!(true));
    let chapters = body["chapterIds"].as_array().unwrap();
    assert_eq!(chapters.len(), 2, "both chapters were purged: {body}");

    // The purge loop then drives the second chapter's purge.
    let (method, _) = tokio::time::timeout(Duration::from_secs(5), node.frames.recv())
        .await?
        .context("second purge frame")?;
    assert_eq!(method, "instance.purge");

    // One audit row, recorded for the successful delete only.
    let db = rusqlite::Connection::open(&ctx.db_path)?;
    let audits: i64 = db.query_row(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'instance.delete'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        audits, 1,
        "exactly one delete audit: no refused-attempt row"
    );
    Ok(())
}
