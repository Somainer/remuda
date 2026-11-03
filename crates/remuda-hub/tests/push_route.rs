//! D-057 §8 (ma-push): push routing for agent-parented alerts, the
//! effectively-running rule and the one transition function, the lift and
//! its outbox, per-device suppression, reply-ready, end evidence for the end
//! pushes, and the audit rows. Fake Nodes, a fake push transport, and every
//! wait is on durable state (no millisecond thresholds).
#![allow(missing_docs)]

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn_with_push};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// The D-051 switch override is process-global: every test here holds this
// lock for its whole life.
static FLAG_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::const_new(()));

struct FlagGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

async fn d051(on: bool) -> FlagGuard {
    let guard = FLAG_LOCK.lock().await;
    remuda_hub::delegated_decisions_test_support::set_global_override(Some(on));
    FlagGuard { _guard: guard }
}

fn set_d051(on: bool) {
    remuda_hub::delegated_decisions_test_support::set_global_override(Some(on));
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        remuda_hub::delegated_decisions_test_support::set_global_override(None);
    }
}

// ─── fake push ──────────────────────────────────────────────────────────────

/// Records every Web Push POST by endpoint.
#[derive(Default)]
struct FakePush {
    hits: Mutex<Vec<(String, Vec<u8>)>>,
    /// While set, every delivery fails (HTTP 400, not retried).
    fail: std::sync::atomic::AtomicBool,
}

impl remuda_push::Transport for FakePush {
    fn post(
        &self,
        url: String,
        _headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u16, remuda_push::Error>> + Send + '_>,
    > {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async { Ok(400) });
        }
        self.hits.lock().unwrap().push((url, body));
        Box::pin(async { Ok(201) })
    }
}

/// A paired device with its own push subscription.
struct Device {
    token: String,
    id: String,
    endpoint: String,
    keys: ece::crypto::EcKeyComponents,
    auth: [u8; 16],
}

// ─── fake Node ──────────────────────────────────────────────────────────────

/// A fake Node that accepts every Hub request and appends journal events in
/// order.
struct FakeNode {
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
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
    async fn connect(hub: &remuda_hub::RunningHub, host: &str, extra: Value) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        Self::connect_with(hub, host, &enroll, extra).await
    }

    async fn connect_with(
        hub: &remuda_hub::RunningHub,
        host: &str,
        bearer: &str,
        extra: Value,
    ) -> Result<Self> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut params = json!({ "hostId": host, "capabilities": {"lineageFence": true},
        "host": {
            "hostname": format!("{host}.push"), "workspaces": [], "workspaceRevision": 0,
            "herdr": {"path": "/usr/bin/herdr"},
            "cli": [{"kind": "claude", "path": "/usr/bin/claude", "auth": "logged_in"}]
        }});
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
        anyhow::ensure!(hello.get("result").is_some(), "hello failed: {hello}");
        let node_token = hello["result"]["nodeToken"].as_str().map(str::to_owned);
        let (appends, mut append_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
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
                        let Some(method) = frame["method"].as_str() else { continue };
                        let id = frame["id"].clone();
                        if id.is_null() { continue; }
                        let reply = if method == "lineage.fence" {
                            json!({"jsonrpc":"2.0","id":id,"result":{
                                "fenceId": frame["params"]["fenceId"], "closedInstanceIds": [],
                                "outcomes": []}})
                        } else {
                            json!({"jsonrpc":"2.0","id":id,"result":{"accepted": true}})
                        };
                        if node.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                    }
                }
            }
        });
        Ok(Self {
            appends,
            node_token,
            task: Some(task),
        })
    }

    fn append(&self, instance_id: &str, event: Value) -> Result<()> {
        self.appends.send((instance_id.to_owned(), event))?;
        Ok(())
    }
}

// ─── harness ────────────────────────────────────────────────────────────────

const EPOCH: &str = "epoch-1";

struct Ctx {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    bootstrap: String,
    hub: remuda_hub::RunningHub,
    push: Arc<FakePush>,
    http: reqwest::Client,
    /// The owner's phone: every API call is made with its token.
    phone: Device,
    /// Host of the parent lineage P.
    host_p: String,
    /// Host of the child I (a separate link, so P's host can go offline).
    host_i: String,
    node_p: Option<FakeNode>,
    node_i: Option<FakeNode>,
    p_node_token: String,
    i_node_token: String,
}

fn hello_extra(epoch: &str, live: &[&str]) -> Value {
    json!({
        "nodeEpoch": epoch,
        "instanceStoreFound": true,
        "instances": live.iter().map(|id| json!({"id": id, "lifecycle": "running"})).collect::<Vec<_>>(),
    })
}

impl Ctx {
    async fn boot() -> Result<Self> {
        Self::boot_with(|_| {}).await
    }

    async fn boot_with(mutate: impl FnOnce(&mut HubConfig)) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let mut config = HubConfig::for_test(data.clone());
        mutate(&mut config);
        let bootstrap = config.bootstrap_token.clone();
        let push = Arc::new(FakePush::default());
        let hub = spawn_with_push(config, push.clone()).await?;
        let http = reqwest::Client::new();
        let phone = Self::device(&hub, &http, "phone").await?;
        let host_p = HostId::new().as_id().as_str().to_owned();
        let host_i = HostId::new().as_id().as_str().to_owned();
        let node_p = FakeNode::connect(&hub, &host_p, hello_extra(EPOCH, &[])).await?;
        let node_i = FakeNode::connect(&hub, &host_i, hello_extra(EPOCH, &[])).await?;
        Ok(Self {
            _dir: dir,
            data,
            bootstrap,
            hub,
            push,
            http,
            phone,
            p_node_token: node_p.node_token.clone().context("node token")?,
            i_node_token: node_i.node_token.clone().context("node token")?,
            host_p,
            host_i,
            node_p: Some(node_p),
            node_i: Some(node_i),
        })
    }

    /// Pair a device and subscribe it to push on its own endpoint.
    async fn device(
        hub: &remuda_hub::RunningHub,
        http: &reqwest::Client,
        name: &str,
    ) -> Result<Device> {
        let token = hub.mint_device_token(name).await?;
        let (pair, auth) = ece::generate_keypair_and_auth_secret().expect("ece");
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let endpoint = format!("http://127.0.0.1:9/push/{name}");
        let subscribed = http
            .post(format!("http://{}/push/subscriptions", hub.addr))
            .bearer_auth(&token)
            .json(&json!({"endpoint": endpoint, "keys": {
                "p256dh": b64.encode(pair.pub_as_raw().expect("pub")),
                "auth": b64.encode(auth)}}))
            .send()
            .await?;
        anyhow::ensure!(subscribed.status() == 201, "{}", subscribed.text().await?);
        let me: Value = http
            .get(format!("http://{}/v1/devices", hub.addr))
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        let id = me["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["name"] == name || item["deviceName"] == name)
            .and_then(|item| item["id"].as_str().or(item["deviceId"].as_str()))
            .context("device id")?
            .to_owned();
        Ok(Device {
            token,
            id,
            endpoint,
            keys: pair.raw_components().expect("components"),
            auth,
        })
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.data.join("hub.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
    }

    fn node_p(&self) -> &FakeNode {
        self.node_p.as_ref().expect("node P")
    }

    fn node_i(&self) -> &FakeNode {
        self.node_i.as_ref().expect("node I")
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{path}", self.base()))
            .bearer_auth(&self.phone.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        anyhow::ensure!(status.is_success(), "POST {path} {status}: {body}");
        Ok(body)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}{path}", self.base()))
            .bearer_auth(&self.phone.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn wait_until<F, Fut, T>(&self, what: &str, probe: F) -> Result<T>
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
        .with_context(|| format!("never held: {what}"))
    }

    fn query_i64(&self, sql: &str, params: impl rusqlite::Params) -> Result<i64> {
        Ok(self.db()?.query_row(sql, params, |row| row.get(0))?)
    }

    /// A continuity parent P (it holds a grant) on host P, reported running.
    async fn parent(&self, running: bool) -> Result<String> {
        let created = self
            .post(
                "/v1/instances",
                json!({
                    "hostId": self.host_p, "kind": "claude", "driver": "claude-sdk",
                    "title": "Coordinator", "grants": ["dispatch"],
                    "permissionMode": "auto", "model": "claude-test-model",
                    "scope": {"hostIds": [self.host_p, self.host_i]}, "prompt": "coordinate",
                }),
            )
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("parent id")?
            .to_owned();
        if running {
            self.report_running(self.node_p(), &id).await?;
            self.wait_lineage(&id, "running").await?;
        }
        Ok(id)
    }

    async fn report_running(&self, node: &FakeNode, id: &str) -> Result<()> {
        node.append(
            id,
            json!({"kind":"lifecycle","payload":{"type":"entity","entityType":"instance",
                "state":"running"}}),
        )?;
        self.wait_until("instance running", || async {
            let lifecycle: Option<String> = self
                .db()
                .ok()?
                .query_row(
                    "SELECT lifecycle FROM instances WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .ok();
            (lifecycle.as_deref() == Some("running")).then_some(())
        })
        .await
    }

    async fn wait_lineage(&self, id: &str, state: &str) -> Result<()> {
        self.wait_until(&format!("lineage {state}"), || async {
            let lineage = self.get(&format!("/v1/lineages/{id}")).await.ok()?;
            (lineage["state"] == state).then_some(())
        })
        .await
    }

    /// A child instance on host I whose parent edge is `parent` (`None`: a
    /// Human-parented instance), reported running.
    async fn child(&self, parent: Option<&str>) -> Result<String> {
        let mut spec = json!({"permissionMode": "manual", "model": "claude-test-model"});
        if let Some(parent) = parent {
            spec["parentInstanceId"] = json!(parent);
        }
        let record = self
            .hub
            .store()
            .context("store")?
            .insert_instance(
                self.host_i.clone(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                spec,
            )
            .await?;
        self.report_running(self.node_i(), &record.instance_id)
            .await?;
        Ok(record.instance_id)
    }

    /// The child raises an interaction of `kind` through its Node.
    fn raise(&self, child: &str, id: &str, kind: &str) -> Result<()> {
        self.node_i().append(
            child,
            json!({"kind":"interaction.requested","payload":{"interaction":{
                "id": id, "kind": kind, "state": "pending", "blocking": true,
                "answerable": true, "resolution": {"state": "unknown"},
                "request": {"kind": kind, "title": "Which way?", "options": []}}}}),
        )
    }

    /// How the alert about `interaction` was decided: `suppressed` (a
    /// suppression row) or `paged` (a push log row for its interaction push).
    async fn decision(&self, interaction: &str) -> Result<&'static str> {
        let tag = format!("interaction:{interaction}");
        self.wait_until("an alert decision", || async {
            let db = self.db().ok()?;
            let suppressed: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM push_suppressions WHERE interaction_id = ?1",
                    [interaction],
                    |row| row.get(0),
                )
                .ok()?;
            let paged: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM push_log WHERE category = 'interaction' AND tag = ?1",
                    [&tag],
                    |row| row.get(0),
                )
                .ok()?;
            match (suppressed, paged) {
                (0, 0) => None,
                (_, 0) => Some("suppressed"),
                _ => Some("paged"),
            }
        })
        .await
    }

    /// Decrypted payloads delivered to `device`, oldest first.
    fn pushes(&self, device: &Device) -> Vec<Value> {
        self.push
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(url, _)| *url == device.endpoint)
            .map(|(_, body)| {
                let plain = ece::decrypt(&device.keys, &device.auth, body).expect("decrypt");
                serde_json::from_slice(&plain).expect("payload")
            })
            .collect()
    }

    fn pushes_tagged(&self, device: &Device, tag: &str) -> Vec<Value> {
        self.pushes(device)
            .into_iter()
            .filter(|payload| payload["tag"] == tag)
            .collect()
    }

    /// Wait until `device` received `count` pushes tagged `tag`.
    async fn wait_pushes(&self, device: &Device, tag: &str, count: usize) -> Result<Vec<Value>> {
        self.wait_until(&format!("{count} push(es) {tag}"), || async {
            let pushes = self.pushes_tagged(device, tag);
            (pushes.len() >= count).then_some(pushes)
        })
        .await
    }

    /// `(lifted_at, lift_cause)` of a suppression row.
    fn suppression(&self, interaction: &str) -> Result<(Option<String>, Option<String>)> {
        Ok(self.db()?.query_row(
            "SELECT lifted_at, lift_cause FROM push_suppressions WHERE interaction_id = ?1",
            [interaction],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    async fn wait_lifted(&self, interaction: &str) -> Result<String> {
        self.wait_until("the lift", || async {
            match self.suppression(interaction).ok()? {
                (Some(_), Some(cause)) => Some(cause),
                _ => None,
            }
        })
        .await
    }

    fn outbox_rows(&self, interaction: &str) -> Result<i64> {
        self.query_i64(
            "SELECT COUNT(*) FROM push_outbox WHERE interaction_id = ?1",
            [interaction],
        )
    }

    async fn close(&self, id: &str) -> Result<()> {
        self.post(
            &format!("/v1/instances/{id}/commands"),
            json!({"operation": "instance.close", "payload": {}}),
        )
        .await?;
        Ok(())
    }

    /// Drop a Node's link and wait until the Hub recorded the loss.
    async fn disconnect(&mut self, host: &str) -> Result<()> {
        if *host == self.host_p {
            self.node_p.take();
        } else {
            self.node_i.take();
        }
        self.wait_until("host offline", || async {
            let state: Option<String> = self
                .db()
                .ok()?
                .query_row("SELECT state FROM hosts WHERE id = ?1", [host], |row| {
                    row.get(0)
                })
                .ok();
            (state.as_deref() != Some("online")).then_some(())
        })
        .await
    }

    /// The parent's chapter ends by itself (Node-attested evidence).
    async fn end_chapter(&self, id: &str) -> Result<()> {
        self.node_p().append(
            id,
            json!({"kind":"lifecycle","payload":{
                "type":"native","topic":"session","nativeName":"session",
                "status":{"state":"known","value":"failed"},"severity":"error",
                "affectsCompletion":true,"relatedIds":{"exitCode":"137","signal":"KILL"}}}),
        )?;
        self.wait_until("chapter ended", || async {
            let lifecycle: Option<String> = self
                .db()
                .ok()?
                .query_row(
                    "SELECT lifecycle FROM instances WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .ok();
            matches!(lifecycle.as_deref(), Some("exited" | "failed")).then_some(())
        })
        .await
    }

    /// Stop the Hub as a crash would leave it: no teardown wrote the hosts
    /// offline or lifted anything (the edit restores that shape), then
    /// `edit`, then start a new Hub on the same data with the same push fake.
    async fn crash_restart(
        &mut self,
        mutate: impl FnOnce(&mut HubConfig),
        edit: impl FnOnce(&rusqlite::Connection) -> Result<()>,
    ) -> Result<()> {
        self.node_p.take();
        self.node_i.take();
        self.hub.shutdown_within(Duration::from_secs(10)).await;
        let db = self.db()?;
        edit(&db)?;
        drop(db);
        let mut config = HubConfig::for_test(self.data.clone());
        config.bootstrap_token = self.bootstrap.clone();
        mutate(&mut config);
        self.hub = spawn_with_push(config, self.push.clone()).await?;
        Ok(())
    }
}

/// The crash shape: hosts still online, the suppressions unlifted and no
/// outbox rows (no teardown ran).
fn as_if_crashed(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch(
        "UPDATE hosts SET state = 'online', offline_since = NULL;
         UPDATE push_suppressions SET lifted_at = NULL, lift_cause = NULL;
         DELETE FROM push_outbox;",
    )?;
    Ok(())
}

/// A suppressed question raised by `child` under a running parent.
async fn suppressed_question(ctx: &Ctx, child: &str, id: &str) -> Result<()> {
    ctx.raise(child, id, "question")?;
    assert_eq!(ctx.decision(id).await?, "suppressed");
    Ok(())
}

// ─── §8.1 routing matrix ────────────────────────────────────────────────────

/// Approval vs question vs plan-review, D-051 on and off, P running,
/// starting, paused, host-offline or with its chapter ended, and a Human
/// parent: only a question or plan review routed (D-051 on) to an
/// effectively running Agent parent is suppressed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_routing_matrix() -> Result<()> {
    let _flag = d051(true).await;
    for parent_state in [
        "running",
        "starting",
        "paused",
        "host-offline",
        "chapter-ended",
    ] {
        let mut ctx = Ctx::boot().await?;
        let parent = ctx.parent(parent_state != "starting").await?;
        let child = ctx.child(Some(&parent)).await?;
        let human_child = ctx.child(None).await?;
        match parent_state {
            "paused" => {
                ctx.close(&parent).await?;
                ctx.wait_lineage(&parent, "paused").await?;
            }
            "host-offline" => {
                let host = ctx.host_p.clone();
                ctx.disconnect(&host).await?;
            }
            "chapter-ended" => ctx.end_chapter(&parent).await?,
            _ => {}
        }
        for on in [true, false] {
            set_d051(on);
            for kind in ["approval", "question", "plan-review"] {
                let id = format!("int_{}_{}_{}", parent_state.replace('-', ""), on, kind);
                let id = id.replace('-', "");
                ctx.raise(&child, &id, kind)?;
                let expected = if parent_state == "running" && on && kind != "approval" {
                    "suppressed"
                } else {
                    "paged"
                };
                assert_eq!(
                    ctx.decision(&id).await?,
                    expected,
                    "{kind}, D-051 {on}, parent {parent_state}"
                );
            }
            // A Human-parented instance pages as today.
            let id = format!("int_human_{}_{}", parent_state.replace('-', ""), on);
            ctx.raise(&human_child, &id, "question")?;
            assert_eq!(ctx.decision(&id).await?, "paged", "Human parent");
        }
        set_d051(true);
        ctx.hub.shutdown().await;
    }
    Ok(())
}

/// The suppression row is the audit row: interaction, lineage, chapter,
/// generation and rule; a suppressed item still appears in /approvals.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suppression_writes_its_audit_row_and_stays_in_approvals() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_audit").await?;
    let row: (String, String, String, String, i64, String, Option<String>) = ctx.db()?.query_row(
        "SELECT instance_id, interaction_kind, lineage_id, chapter_instance_id,
                    chapter_generation, rule, lifted_at
               FROM push_suppressions WHERE interaction_id = 'int_audit'",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    assert_eq!(
        row,
        (
            child.clone(),
            "question".to_owned(),
            parent.clone(),
            parent.clone(),
            1,
            "routed-to-running-parent".to_owned(),
            None
        )
    );
    let listed = ctx.get("/v1/interactions").await?;
    let items = listed["items"].as_array().context("items")?;
    assert!(
        items
            .iter()
            .any(|item| item["id"] == "int_audit" || item["interactionId"] == "int_audit"),
        "a suppressed item stays in /approvals: {listed}"
    );
    assert!(ctx.pushes(&ctx.phone).is_empty(), "nothing was pushed");
    Ok(())
}

// ─── §8.3 the lift, per cause ───────────────────────────────────────────────

/// F (a Human pause) lifts the suppressed question: one outbox row, one push
/// with today's body and the interaction's tag. A second transition (the
/// parent's host link going down) adds nothing, and the lifted interaction
/// is never suppressed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fence_lifts_once_and_a_second_transition_adds_nothing() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_fence").await?;
    // An answered suppressed item is moot: it is not lifted.
    suppressed_question(&ctx, &child, "int_answered").await?;
    ctx.db()?.execute(
        "UPDATE interactions SET state = 'resolved' WHERE id = 'int_answered'",
        [],
    )?;
    ctx.close(&parent).await?;
    assert_eq!(ctx.wait_lifted("int_fence").await?, "fence");
    let pushes = ctx
        .wait_pushes(&ctx.phone, "interaction:int_fence", 1)
        .await?;
    assert_eq!(pushes[0]["title"], "Need your input");
    assert_eq!(pushes[0]["body"], "An approval or question is waiting.");
    assert_eq!(pushes[0]["data"]["url"], "/sessions?focus=int_fence");
    assert_eq!(ctx.suppression("int_answered")?, (None, None));
    // A second transition: P's host link goes down.
    let host = ctx.host_p.clone();
    ctx.disconnect(&host).await?;
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(ctx.outbox_rows("int_fence")?, 1);
    assert_eq!(ctx.outbox_rows("int_answered")?, 0);
    assert_eq!(
        ctx.pushes_tagged(&ctx.phone, "interaction:int_fence").len(),
        1
    );
    assert_eq!(ctx.suppression("int_fence")?.1.as_deref(), Some("fence"));
    // Never re-suppressed: the same interaction alerting again pages.
    ctx.raise(&child, "int_fence", "question")?;
    ctx.wait_until("a page for the lifted item", || async {
        let paged = ctx
            .query_i64(
                "SELECT COUNT(*) FROM push_log WHERE category = 'interaction'
                  AND tag = 'interaction:int_fence'",
                [],
            )
            .ok()?;
        (paged > 0).then_some(())
    })
    .await?;
    Ok(())
}

/// Host link loss lifts (the WS session end reaches `mark_host_offline`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_link_loss_lifts() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_link").await?;
    let host = ctx.host_p.clone();
    ctx.disconnect(&host).await?;
    assert_eq!(ctx.wait_lifted("int_link").await?, "link-lost");
    ctx.wait_pushes(&ctx.phone, "interaction:int_link", 1)
        .await?;
    // A new question while P's host is offline pages directly.
    ctx.raise(&child, "int_link_2", "question")?;
    assert_eq!(ctx.decision("int_link_2").await?, "paged");
    Ok(())
}

/// The chapter's recorded end lifts before the supervisor acts (§8.2), and
/// the supervisor's own transition lifts an end it finds on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chapter_end_and_the_supervisor_lift() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_end").await?;
    ctx.end_chapter(&parent).await?;
    assert_eq!(ctx.wait_lifted("int_end").await?, "chapter-ended");
    ctx.wait_pushes(&ctx.phone, "interaction:int_end", 1)
        .await?;

    // The supervisor: an end recorded without a journal event.
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_supervisor").await?;
    ctx.hub.test_mark_instance_exited(&parent).await?;
    assert_eq!(ctx.suppression("int_supervisor")?, (None, None));
    let decided = ctx.hub.test_handle().test_supervise().await;
    assert!(
        decided
            .iter()
            .any(|decision| decision["lineageId"] == json!(parent)),
        "{decided:?}"
    );
    assert_eq!(ctx.wait_lifted("int_supervisor").await?, "supervisor");
    ctx.wait_pushes(&ctx.phone, "interaction:int_supervisor", 1)
        .await?;
    Ok(())
}

// ─── §8.3 boot ──────────────────────────────────────────────────────────────

/// Boot step 2: a lineage not running by state is lifted at boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_lifts_a_lineage_not_running_by_state() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_boot").await?;
    let ended = parent.clone();
    ctx.crash_restart(
        |_| {},
        move |db| {
            as_if_crashed(db)?;
            db.execute(
                "UPDATE instances SET lifecycle = 'exited', ended_at = '2026-11-01T00:00:00.000Z'
                  WHERE id = ?1",
                [ended],
            )?;
            Ok(())
        },
    )
    .await?;
    assert_eq!(ctx.wait_lifted("int_boot").await?, "boot");
    ctx.wait_pushes(&ctx.phone, "interaction:int_boot", 1)
        .await?;
    Ok(())
}

/// Boot step 3, the hello path: the parent's host says hello again with the
/// same epoch and the chapter in its inventory, so nothing is lifted; a
/// hello with a new epoch that no longer holds the chapter lifts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_waits_for_the_parent_hosts_hello() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_hello").await?;
    ctx.crash_restart(|_| {}, as_if_crashed).await?;
    // Every host reads offline at boot: bookkeeping, not an observation.
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(ctx.suppression("int_hello")?, (None, None));
    // Same epoch, the chapter held: P is still effectively running.
    let node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host_p,
        &ctx.p_node_token,
        hello_extra(EPOCH, &[&parent]),
    )
    .await?;
    assert_eq!(ctx.suppression("int_hello")?, (None, None));
    assert_eq!(ctx.outbox_rows("int_hello")?, 0);
    drop(node);
    // Crash again; this time the Node restarted and no longer holds P.
    ctx.crash_restart(|_| {}, as_if_crashed).await?;
    let _node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host_p,
        &ctx.p_node_token,
        hello_extra("epoch-2", &[]),
    )
    .await?;
    assert_eq!(ctx.wait_lifted("int_hello").await?, "hello");
    ctx.wait_pushes(&ctx.phone, "interaction:int_hello", 1)
        .await?;
    Ok(())
}

/// A same-epoch hello whose inventory lacks the chapter lifts too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_epoch_hello_without_the_chapter_lifts() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_absent").await?;
    ctx.crash_restart(|_| {}, as_if_crashed).await?;
    let _node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host_p,
        &ctx.p_node_token,
        hello_extra(EPOCH, &[]),
    )
    .await?;
    assert_eq!(ctx.wait_lifted("int_absent").await?, "hello");
    Ok(())
}

/// Boot step 3, the grace path: the parent's host never says hello, and
/// host-loss detection under the existing grace lifts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_lifts_on_host_loss_under_the_grace() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_grace").await?;
    let host = ctx.host_p.clone();
    ctx.crash_restart(
        |_| {},
        move |db| {
            as_if_crashed(db)?;
            // The injected clock: the host was last seen long before the
            // grace, which this Hub keeps at its default.
            db.execute(
                "UPDATE hosts SET last_seen_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
                [host],
            )?;
            Ok(())
        },
    )
    .await?;
    assert_eq!(ctx.wait_lifted("int_grace").await?, "host-lost");
    ctx.wait_pushes(&ctx.phone, "interaction:int_grace", 1)
        .await?;
    Ok(())
}

/// A crash between send and mark: the boot drive resends the undelivered
/// row, with the same tag, so it replaces the first notification. A row
/// whose fanout failed is also sent after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_between_send_and_mark_resends_with_the_same_tag() -> Result<()> {
    let _flag = d051(true).await;
    let mut ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_crash").await?;
    ctx.hub.test_handle().test_skip_outbox_marks(1);
    ctx.close(&parent).await?;
    let first = ctx
        .wait_pushes(&ctx.phone, "interaction:int_crash", 1)
        .await?;
    let unsent = ctx.query_i64(
        "SELECT COUNT(*) FROM push_outbox WHERE interaction_id = 'int_crash' AND recipients IS NULL",
        [],
    )?;
    assert_eq!(unsent, 1, "sent but never marked");
    ctx.crash_restart(|_| {}, |_| Ok(())).await?;
    let both = ctx
        .wait_pushes(&ctx.phone, "interaction:int_crash", 2)
        .await?;
    assert_eq!(both[0]["tag"], first[0]["tag"]);
    assert_eq!(both[1]["tag"], "interaction:int_crash");
    ctx.wait_until("marked delivered", || async {
        let left = ctx
            .query_i64(
                "SELECT COUNT(*) FROM push_outbox WHERE recipients IS NULL",
                [],
            )
            .ok()?;
        (left == 0).then_some(())
    })
    .await?;

    // A failed fanout leaves the row undelivered; it is sent after restart.
    let node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host_p,
        &ctx.p_node_token,
        hello_extra(EPOCH, &[]),
    )
    .await?;
    ctx.node_p = Some(node);
    let node = FakeNode::connect_with(
        &ctx.hub,
        &ctx.host_i,
        &ctx.i_node_token,
        hello_extra(EPOCH, &[]),
    )
    .await?;
    ctx.node_i = Some(node);
    let parent = ctx.parent(true).await?;
    let child = ctx.child(Some(&parent)).await?;
    suppressed_question(&ctx, &child, "int_failed").await?;
    ctx.push
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    ctx.close(&parent).await?;
    ctx.wait_lifted("int_failed").await?;
    ctx.hub.test_handle().test_drive_outbox().await;
    assert!(
        ctx.pushes_tagged(&ctx.phone, "interaction:int_failed")
            .is_empty()
    );
    ctx.push
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ctx.crash_restart(|_| {}, |_| Ok(())).await?;
    ctx.wait_pushes(&ctx.phone, "interaction:int_failed", 1)
        .await?;
    Ok(())
}

// ─── §8.4 per device ────────────────────────────────────────────────────────

async fn follow(
    ctx: &Ctx,
    device: &Device,
    instance: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let mut request =
        format!("ws://{}/v1/follow?instanceId={instance}", ctx.hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", device.token).parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
    // The snapshot proves the Hub registered the follow.
    let first = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await?
        .context("snapshot")??;
    anyhow::ensure!(matches!(first, Message::Text(_)), "{first:?}");
    Ok(ws)
}

/// Send a visibility frame and prove the Hub applied it: a `subscribe` on
/// the same socket is read after it, and its snapshot comes back.
async fn report_visibility<S>(ws: &mut S, instance: &str, state: &str) -> Result<()>
where
    S: SinkExt<Message>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures::Sink<Message>>::Error: std::fmt::Debug,
{
    ws.send(Message::Text(
        json!({"type": "visibility", "state": state})
            .to_string()
            .into(),
    ))
    .await
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    ws.send(Message::Text(
        json!({"type": "subscribe", "instanceIds": [instance]})
            .to_string()
            .into(),
    ))
    .await
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await?
        .context("snapshot")??;
    Ok(())
}

/// A follow suppresses pushes only for its own device and only while that
/// device reports the page visible; a client that never sends the frame
/// counts as visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_mutes_only_its_own_visible_device() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let desktop = Ctx::device(&ctx.hub, &ctx.http, "desktop").await?;
    let tablet = Ctx::device(&ctx.hub, &ctx.http, "tablet").await?;
    let instance = ctx.child(None).await?;
    // The desktop follows and never sends the frame: visible.
    let mut desktop_ws = follow(&ctx, &desktop, &instance).await?;
    ctx.raise(&instance, "int_dev_1", "approval")?;
    ctx.wait_pushes(&ctx.phone, "interaction:int_dev_1", 1)
        .await?;
    ctx.wait_pushes(&tablet, "interaction:int_dev_1", 1).await?;
    assert!(
        ctx.pushes_tagged(&desktop, "interaction:int_dev_1")
            .is_empty()
    );
    let suppressed: String = ctx.db()?.query_row(
        "SELECT suppressed_devices_json FROM push_log WHERE tag = 'interaction:int_dev_1'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(suppressed, json!([desktop.id]).to_string());
    // The desktop's page goes hidden: it is pushed again.
    report_visibility(&mut desktop_ws, &instance, "hidden").await?;
    ctx.raise(&instance, "int_dev_2", "approval")?;
    ctx.wait_pushes(&desktop, "interaction:int_dev_2", 1)
        .await?;
    // Shown again: muted again, and only the desktop.
    report_visibility(&mut desktop_ws, &instance, "visible").await?;
    // The tablet follows too and hides its page.
    let mut tablet_ws = follow(&ctx, &tablet, &instance).await?;
    report_visibility(&mut tablet_ws, &instance, "hidden").await?;
    ctx.raise(&instance, "int_dev_3", "approval")?;
    ctx.wait_pushes(&ctx.phone, "interaction:int_dev_3", 1)
        .await?;
    ctx.wait_pushes(&tablet, "interaction:int_dev_3", 1).await?;
    assert!(
        ctx.pushes_tagged(&desktop, "interaction:int_dev_3")
            .is_empty()
    );
    Ok(())
}

// ─── §8.5 the seat's own pushes ─────────────────────────────────────────────

/// A Main seat on host P, reported running; returns its id.
async fn seat(ctx: &Ctx) -> Result<String> {
    let created = ctx
        .post(
            "/v1/instances",
            json!({
                "hostId": ctx.host_p, "kind": "claude", "driver": "claude-sdk",
                "name": "main", "title": "Main", "grants": ["address-owner"],
                "permissionMode": "auto", "model": "claude-test-model",
                "restart": {"onProcessLoss": true, "maxPerHour": 3},
                "prompt": "seat brief",
            }),
        )
        .await?;
    let id = created["instance"]["instanceId"]
        .as_str()
        .context("seat")?
        .to_owned();
    ctx.report_running(ctx.node_p(), &id).await?;
    ctx.wait_lineage(&id, "running").await?;
    Ok(id)
}

fn user_message(command: &str) -> Value {
    json!({"kind":"message","payload":{"operation":"open","messageId":format!("msg_{command}"),
        "role":"user","phase":"final","blocks":[{"type":"text","text":"hello"}],
        "parentToolCallId":null,"commandId":command}})
}

fn turn_started(command: &str) -> Value {
    json!({"kind":"lifecycle","payload":{"type":"native","topic":"turn","nativeName":"turn_started",
        "nativeId":{"state":"known","value":"session-1"},
        "status":{"state":"known","value":"working"},
        "relatedIds":{"nativeClientMessageId":command},"severity":"info","affectsCompletion":false}})
}

fn assistant(id: &str, operation: &str, text: &str) -> Value {
    json!({"kind":"message","payload":{"operation":operation,"messageId":id,"role":"assistant",
        "phase":"final","blocks":[{"type":"text","text":text}],"parentToolCallId":null}})
}

fn turn_settled(command: &str) -> Value {
    json!({"kind":"lifecycle","payload":{"type":"native","topic":"turn","nativeName":"result",
        "nativeId":{"state":"known","value":"session-1"},
        "status":{"state":"known","value":"turn_done"},
        "relatedIds":{"completedTurn":command,"settledRootTurn":"true"},
        "severity":"info","affectsCompletion":true}})
}

/// Run one turn of `seat` for the input `command`, ending with `reply`.
async fn turn(ctx: &Ctx, seat: &str, command: &str, reply: &str) -> Result<()> {
    let node = ctx.node_p();
    node.append(seat, user_message(command))?;
    node.append(seat, turn_started(command))?;
    node.append(seat, assistant(&format!("a_{command}"), "open", "Here"))?;
    node.append(seat, assistant(&format!("a_{command}"), "replace", reply))?;
    node.append(seat, turn_settled(command))?;
    ctx.wait_until("the turn ended", || async {
        let activity: Option<String> = ctx
            .db()
            .ok()?
            .query_row(
                "SELECT activity FROM instances WHERE id = ?1",
                [seat],
                |row| row.get(0),
            )
            .ok();
        let reached: i64 = ctx
            .db()
            .ok()?
            .query_row(
                "SELECT COUNT(*) FROM journal WHERE instance_id = ?1
                  AND json_extract(payload_json, '$.payload.nativeName') = 'result'
                  AND json_extract(payload_json, '$.payload.relatedIds.completedTurn') = ?2",
                [seat, command],
                |row| row.get(0),
            )
            .ok()?;
        (reached > 0 && activity.as_deref() == Some("idle")).then_some(())
    })
    .await
}

/// A Human input to Main: an `instance.send` from the phone; returns its
/// command id.
async fn human_send(ctx: &Ctx, seat: &str) -> Result<String> {
    let sent = ctx
        .post(
            &format!("/v1/instances/{seat}/commands"),
            json!({"operation": "instance.send",
                   "payload": {"input": {"type": "prompt", "text": "status?"}}}),
        )
        .await?;
    Ok(sent["commandId"]
        .as_str()
        .or(sent["command"]["commandId"].as_str())
        .or(sent["id"].as_str())
        .context("command id")?
        .to_owned())
}

/// Reply-ready: a turn of the live address-owner chapter reached by a Human
/// input pushes "Main replied: <first line>", per-device suppression
/// applies; a turn reached only by Agent or Hub input does not push.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_ready_only_for_turns_reached_by_human_input() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let desktop = Ctx::device(&ctx.hub, &ctx.http, "desktop").await?;
    let main = seat(&ctx).await?;
    let tag = format!("instance:{main}");
    // The desktop has /s/:main visible.
    let _desktop_ws = follow(&ctx, &desktop, &main).await?;
    let command = human_send(&ctx, &main).await?;
    turn(
        &ctx,
        &main,
        &command,
        "All three workers are green.\nDetails follow.",
    )
    .await?;
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(
        pushes[0]["title"],
        "Main replied: All three workers are green."
    );
    assert_eq!(pushes[0]["data"]["url"], format!("/s/{main}"));
    assert!(
        ctx.pushes_tagged(&desktop, &tag).is_empty(),
        "the desktop showing Main is not pushed"
    );
    // Agent- and Hub-origin inputs: no push.
    let db = ctx.db()?;
    for (id, origin) in [("cmd_agent_in", "agent"), ("cmd_hub_in", "hub")] {
        db.execute(
            "INSERT INTO commands (id, instance_id, host_id, operation, state, resolution,
                forwarded, payload_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'instance.send', 'settled', 'clear', 1, ?4, ?5, ?5)",
            rusqlite::params![
                id,
                main,
                ctx.host_p,
                json!({"origin": origin, "input": {"type": "prompt", "text": "x"}}).to_string(),
                "2026-11-01T00:00:00.000Z"
            ],
        )?;
        turn(&ctx, &main, id, "not for the owner").await?;
    }
    // A turn with no Remuda input at all (the carrier's own turn).
    turn(&ctx, &main, "native-turn", "background work done").await?;
    ctx.hub.test_handle().test_drive_outbox().await;
    let replies = ctx.query_i64(
        "SELECT COUNT(*) FROM push_outbox WHERE kind = 'reply' AND instance_id = ?1",
        [&main],
    )?;
    assert_eq!(replies, 1, "only the Human-reached turn queued a reply");
    assert_eq!(ctx.pushes_tagged(&ctx.phone, &tag).len(), 1);
    Ok(())
}

/// `(operation, payload origin, initiator instance, launch prompt)` of a
/// command row as the Hub stored it.
fn command_row(ctx: &Ctx, id: &str) -> Result<(String, String, Option<String>, String)> {
    Ok(ctx.db()?.query_row(
        "SELECT operation, json_extract(payload_json, '$.origin'), initiator_instance_id,
                json_extract(payload_json, '$.initialInput.text')
           FROM commands WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?)
}

/// Reply rows the outbox holds for `instance`.
fn reply_rows(ctx: &Ctx, instance: &str) -> Result<i64> {
    ctx.query_i64(
        "SELECT COUNT(*) FROM push_outbox WHERE kind = 'reply' AND instance_id = ?1",
        [instance],
    )
}

/// r2 item 1: Main seated through the real Human create path. Its launch
/// prompt rides the create command (stamped `human` from the device, no
/// initiator), the Node journals that command's user message, and the end of
/// the turn it reached pushes exactly one "Main replied".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_human_created_seats_first_turn_pushes_main_replied() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let created = ctx
        .post(
            "/v1/instances",
            json!({
                "hostId": ctx.host_p, "kind": "claude", "driver": "claude-sdk",
                "name": "main", "title": "Main", "grants": ["address-owner"],
                "permissionMode": "auto", "model": "claude-test-model",
                "restart": {"onProcessLoss": true, "maxPerHour": 3},
                "prompt": "Owner's opening brief for Main.",
            }),
        )
        .await?;
    let main = created["instance"]["instanceId"]
        .as_str()
        .context("seat")?
        .to_owned();
    let create = created["command"]["commandId"]
        .as_str()
        .context("create command")?
        .to_owned();
    assert_eq!(
        command_row(&ctx, &create)?,
        (
            "instance.create".to_owned(),
            "human".to_owned(),
            None,
            "Owner's opening brief for Main.".to_owned()
        ),
        "the create command as the Hub stored it"
    );
    ctx.report_running(ctx.node_p(), &main).await?;
    ctx.wait_lineage(&main, "running").await?;
    turn(&ctx, &main, &create, "Seated and ready.\nDetails follow.").await?;
    let tag = format!("instance:{main}");
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(pushes[0]["title"], "Main replied: Seated and ready.");
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(reply_rows(&ctx, &main)?, 1);
    assert_eq!(ctx.pushes_tagged(&ctx.phone, &tag).len(), 1);
    Ok(())
}

/// r2 item 1: a seat an Agent created. Its create command (the real Agent
/// shape: stamped `agent`, with the creator as initiator) reaches its first
/// turn, which pushes nothing. An Agent can only delegate grants it holds and
/// `address-owner` is one per Hub, so the grant is written onto the
/// Agent-created continuity chapter directly. A later Human input to the
/// same seat does push, so the Agent origin is the only difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_created_seats_first_turn_pushes_nothing() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    let token = ctx
        .post(&format!("/v1/instances/{parent}/mcp-token"), json!({}))
        .await?["token"]
        .as_str()
        .context("agent token")?
        .to_owned();
    let response = ctx
        .http
        .post(format!("{}/v1/instances", ctx.base()))
        .bearer_auth(&token)
        .json(&json!({
            "hostId": ctx.host_p, "kind": "claude", "driver": "claude-sdk",
            "title": "Agent seat", "grants": ["dispatch"],
            "permissionMode": "manual", "model": "claude-test-model",
            "scope": {"hostIds": [ctx.host_p]},
            "prompt": "The coordinator's launch prompt.",
        }))
        .send()
        .await?;
    let status = response.status();
    let created: Value = response.json().await?;
    anyhow::ensure!(status.is_success(), "agent create {status}: {created}");
    let seat = created["instance"]["instanceId"]
        .as_str()
        .context("seat")?
        .to_owned();
    let create = created["command"]["commandId"]
        .as_str()
        .context("create command")?
        .to_owned();
    assert_eq!(
        command_row(&ctx, &create)?,
        (
            "instance.create".to_owned(),
            "agent".to_owned(),
            Some(parent.clone()),
            "The coordinator's launch prompt.".to_owned()
        ),
        "the Agent create command as the Hub stored it"
    );
    ctx.db()?.execute(
        "UPDATE instances SET grants_json = '[\"address-owner\",\"dispatch\"]' WHERE id = ?1",
        [&seat],
    )?;
    ctx.report_running(ctx.node_p(), &seat).await?;
    ctx.wait_lineage(&seat, "running").await?;
    turn(&ctx, &seat, &create, "Agent-started turn done.").await?;
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(
        reply_rows(&ctx, &seat)?,
        0,
        "an Agent-created first turn pushes nothing"
    );
    let tag = format!("instance:{seat}");
    assert!(ctx.pushes_tagged(&ctx.phone, &tag).is_empty());
    // The same seat answering the owner pushes.
    let human = human_send(&ctx, &seat).await?;
    turn(&ctx, &seat, &human, "Answer for the owner.").await?;
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(pushes[0]["title"], "Main replied: Answer for the owner.");
    Ok(())
}

/// Wait until `seat`'s root turn for `command` has settled and the seat
/// reads idle.
async fn turn_settled_for(ctx: &Ctx, seat: &str, command: &str) -> Result<()> {
    ctx.wait_until("the turn ended", || async {
        let db = ctx.db().ok()?;
        let activity: String = db
            .query_row(
                "SELECT activity FROM instances WHERE id = ?1",
                [seat],
                |row| row.get(0),
            )
            .ok()?;
        let reached: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM journal WHERE instance_id = ?1
                  AND json_extract(payload_json, '$.payload.nativeName') = 'result'
                  AND json_extract(payload_json, '$.payload.relatedIds.completedTurn') = ?2",
                [seat, command],
                |row| row.get(0),
            )
            .ok()?;
        (reached > 0 && activity == "idle").then_some(())
    })
    .await
}

/// r2 item 2: a Human input that lands while an earlier (Agent) turn runs is
/// answered by its own turn. The earlier turn's end names that turn
/// (`completedTurn`), so it leaves the Human input's tracker armed and
/// pushes nothing; the turn that answers the owner pushes, with its own
/// final message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_human_input_mid_turn_is_pushed_by_the_turn_that_answers_it() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let main = seat(&ctx).await?;
    let tag = format!("instance:{main}");
    ctx.db()?.execute(
        "INSERT INTO commands (id, instance_id, host_id, operation, state, resolution,
            forwarded, payload_json, created_at, updated_at)
         VALUES ('cmd_agent_mid', ?1, ?2, 'instance.send', 'settled', 'clear', 1, ?3, ?4, ?4)",
        rusqlite::params![
            main,
            ctx.host_p,
            json!({"origin": "agent", "input": {"type": "prompt", "text": "x"}}).to_string(),
            "2026-11-01T00:00:00.000Z"
        ],
    )?;
    let node = ctx.node_p();
    node.append(&main, user_message("cmd_agent_mid"))?;
    node.append(&main, turn_started("cmd_agent_mid"))?;
    node.append(
        &main,
        assistant("a_agent", "open", "Working on the agent's ask."),
    )?;
    // The owner's input arrives while that turn runs.
    let human = human_send(&ctx, &main).await?;
    node.append(&main, user_message(&human))?;
    node.append(&main, turn_settled("cmd_agent_mid"))?;
    turn_settled_for(&ctx, &main, "cmd_agent_mid").await?;
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(
        reply_rows(&ctx, &main)?,
        0,
        "the earlier turn's end pushes nothing"
    );
    assert_eq!(
        ctx.query_i64(
            "SELECT COUNT(*) FROM push_reply_trackers WHERE instance_id = ?1 AND command_id = ?2",
            [&main, &human],
        )?,
        1,
        "the owner's input stays armed"
    );
    // The turn that answers the owner.
    node.append(&main, turn_started(&human))?;
    node.append(
        &main,
        assistant("a_owner", "open", "Here is the answer you asked for."),
    )?;
    node.append(&main, turn_settled(&human))?;
    turn_settled_for(&ctx, &main, &human).await?;
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(
        pushes[0]["title"],
        "Main replied: Here is the answer you asked for."
    );
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(reply_rows(&ctx, &main)?, 1);
    assert_eq!(ctx.pushes_tagged(&ctx.phone, &tag).len(), 1);
    Ok(())
}

/// End pushes need process-end evidence (OA6): "Main closed itself" waits
/// for the end its close brings, and an exited event that ends nothing (a
/// subagent's) sends no exited push.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_pushes_need_process_end_evidence() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    // A plain session: a subagent's "exited" is no end of the instance.
    let plain = ctx.child(None).await?;
    let exited_tag = format!("instance:{plain}");
    ctx.node_i().append(
        &plain,
        json!({"kind":"lifecycle","lifecycle":"exited","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"exited"},"severity":"info",
            "affectsCompletion":false,"relatedIds":{"agentId":"sub-1"}}}),
    )?;
    // Its real end pushes "Session exited".
    ctx.node_i().append(
        &plain,
        json!({"kind":"lifecycle","lifecycle":"exited","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"exited"},"severity":"info",
            "affectsCompletion":true,"relatedIds":{"exitCode":"0"}}}),
    )?;
    let exited = ctx.wait_pushes(&ctx.phone, &exited_tag, 1).await?;
    assert_eq!(exited[0]["title"], "Session exited");
    assert_eq!(
        ctx.query_i64(
            "SELECT COUNT(*) FROM push_log WHERE category = 'exited' AND instance_id = ?1",
            [&plain],
        )?,
        1,
        "the subagent's exited event pushed nothing"
    );

    // Main closes itself: recorded at admission, pushed only after its end.
    let main = seat(&ctx).await?;
    let token = ctx
        .post(&format!("/v1/instances/{main}/mcp-token"), json!({}))
        .await?["token"]
        .as_str()
        .context("seat token")?
        .to_owned();
    let closed = ctx
        .http
        .post(format!("{}/v1/instances/{main}/commands", ctx.base()))
        .bearer_auth(&token)
        .json(&json!({"operation": "instance.close", "payload": {}}))
        .send()
        .await?;
    anyhow::ensure!(closed.status().is_success(), "{}", closed.text().await?);
    ctx.wait_lineage(&main, "paused").await?;
    let lineage_tag = format!("instance:{main}");
    let recorded = ctx.query_i64(
        "SELECT COUNT(*) FROM lineage_c1_pushes WHERE lineage_id = ?1 AND kind = 'self'",
        [&main],
    )?;
    assert_eq!(recorded, 1);
    ctx.hub.test_handle().test_supervise().await;
    assert!(
        ctx.pushes_tagged(&ctx.phone, &lineage_tag)
            .iter()
            .all(|push| push["title"] != "Main closed itself"),
        "no C1 push before the end"
    );
    // The close ends the process.
    ctx.node_p().append(
        &main,
        json!({"kind":"lifecycle","lifecycle":"exited","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"exited"},"severity":"info",
            "affectsCompletion":true,"relatedIds":{"exitCode":"0"}}}),
    )?;
    let pushes = ctx
        .wait_until("the C1 push", || async {
            let pushes: Vec<Value> = ctx
                .pushes_tagged(&ctx.phone, &lineage_tag)
                .into_iter()
                .filter(|push| push["title"] == "Main closed itself")
                .collect();
            (!pushes.is_empty()).then_some(pushes)
        })
        .await?;
    assert_eq!(pushes.len(), 1);
    // A restart lineage gets no generic exited push.
    assert!(
        ctx.pushes_tagged(&ctx.phone, &lineage_tag)
            .iter()
            .all(|push| push["title"] != "Session exited")
    );
    Ok(())
}

/// r2 item 3: a store read error while checking the end evidence fails
/// open: the end push is sent (and logged), never silently dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_end_push_survives_a_failed_evidence_read() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let plain = ctx.child(None).await?;
    remuda_hub::push_test_support::arm_end_evidence_read_failure(&plain);
    ctx.node_i().append(
        &plain,
        json!({"kind":"lifecycle","lifecycle":"exited","payload":{
            "type":"native","topic":"session","nativeName":"session",
            "status":{"state":"known","value":"exited"},"severity":"info",
            "affectsCompletion":true,"relatedIds":{"exitCode":"0"}}}),
    )?;
    let tag = format!("instance:{plain}");
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(pushes[0]["title"], "Session exited");
    assert_eq!(
        ctx.query_i64(
            "SELECT COUNT(*) FROM push_log WHERE category = 'exited' AND instance_id = ?1",
            [&plain],
        )?,
        1
    );
    Ok(())
}

/// The push log carries categories, tags and counts, never a body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_push_log_records_categories_and_counts() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let instance = ctx.child(None).await?;
    ctx.raise(&instance, "int_log", "approval")?;
    ctx.wait_pushes(&ctx.phone, "interaction:int_log", 1)
        .await?;
    let row: (String, String, String, i64, String) = ctx.db()?.query_row(
        "SELECT category, outcome, instance_id, sent, delivered_devices_json
           FROM push_log WHERE tag = 'interaction:int_log'",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    assert_eq!(
        row,
        (
            "interaction".to_owned(),
            "sent".to_owned(),
            instance.clone(),
            1,
            json!([ctx.phone.id]).to_string()
        )
    );
    let columns: Vec<String> = ctx
        .db()?
        .prepare("SELECT name FROM pragma_table_info('push_log')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert!(
        !columns.iter().any(|name| name == "body" || name == "title"),
        "no body or title column: {columns:?}"
    );
    Ok(())
}

// ─── cb-inbox-deliver: a fallback page honours §8.1 ─────────────────────────

/// The parent's session reports `state` (activity), naming its native session.
fn session(state: &str) -> Value {
    json!({"kind":"lifecycle","payload":{"type":"native","topic":"session","nativeName":"session",
        "nativeId":{"state":"known","value":"session-1"},
        "status":{"state":"known","value":state},"relatedIds":{},"dataRef":null,
        "severity":"info","affectsCompletion":false}})
}

impl Ctx {
    async fn wait_activity(&self, id: &str, activity: &str) -> Result<()> {
        self.wait_until(&format!("activity {activity}"), || async {
            let now: Option<String> = self
                .db()
                .ok()?
                .query_row(
                    "SELECT activity FROM instances WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .ok();
            (now.as_deref() == Some(activity)).then_some(())
        })
        .await
    }

    /// The inbox row about `interaction` and, once it fell back, the owner
    /// push it owes: `(fact id, fallback reason, push recipients)`.
    fn fallback(&self, interaction: &str) -> Result<(i64, Option<String>, Option<i64>)> {
        Ok(self.db()?.query_row(
            "SELECT f.id, f.fallback_reason, p.recipients FROM inbox_facts f
               LEFT JOIN inbox_fallback_pushes p ON p.fact_id = f.id
              WHERE f.kind = 'interaction'
                AND json_extract(f.payload_json, '$.interactionId') = ?1",
            [interaction],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }

    async fn mute_interactions(&self, lineage: &str, muted: bool) -> Result<()> {
        let kinds = if muted {
            json!(["interaction"])
        } else {
            json!([])
        };
        let response = self
            .http
            .patch(format!("{}/v1/lineages/{lineage}/inbox", self.base()))
            .bearer_auth(&self.phone.token)
            .json(&json!({ "mutedKinds": kinds }))
            .send()
            .await?;
        anyhow::ensure!(response.status().is_success(), "mute");
        Ok(())
    }
}

/// An inbox row that falls back to the owner push (its kind muted after it
/// was written) does not page an interaction §8 owns. Routed to an
/// effectively running parent, the fallback pages nothing; once the parent
/// stops running the lift pages it once, and a fallback after that (in the
/// resumed chapter) adds no second page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fallback_page_honours_the_routing_and_the_lift_pages_once() -> Result<()> {
    let _flag = d051(true).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.parent(true).await?;
    ctx.node_p().append(&parent, session("working"))?;
    ctx.wait_activity(&parent, "working").await?;
    let child = ctx.child(Some(&parent)).await?;

    // Routed and running: suppressed at event time; muted before the batch
    // forms, so the row falls back, and the fallback pages nothing.
    suppressed_question(&ctx, &child, "int_fb_running").await?;
    ctx.wait_until("the fact", || async {
        ctx.fallback("int_fb_running").ok().map(|_| ())
    })
    .await?;
    ctx.mute_interactions(&parent, true).await?;
    ctx.node_p().append(&parent, session("idle"))?;
    let (_, reason, recipients) = ctx
        .wait_until("the fallback settled", || async {
            let row = ctx.fallback("int_fb_running").ok()?;
            row.2.is_some().then_some(row)
        })
        .await?;
    assert_eq!(reason.as_deref(), Some("muted"));
    assert_eq!(recipients, Some(0), "no page: §8.1 routes it");
    assert!(
        ctx.pushes_tagged(&ctx.phone, "interaction:int_fb_running")
            .is_empty()
    );

    // A second question, suppressed the same way, still waiting.
    ctx.mute_interactions(&parent, false).await?;
    ctx.node_p().append(&parent, session("working"))?;
    ctx.wait_activity(&parent, "working").await?;
    suppressed_question(&ctx, &child, "int_fb_lifted").await?;
    ctx.wait_until("the second fact", || async {
        ctx.fallback("int_fb_lifted").ok().map(|_| ())
    })
    .await?;

    // The parent stops running: the lift pages each once.
    ctx.close(&parent).await?;
    ctx.wait_lineage(&parent, "paused").await?;
    ctx.wait_pushes(&ctx.phone, "interaction:int_fb_running", 1)
        .await?;
    ctx.wait_pushes(&ctx.phone, "interaction:int_fb_lifted", 1)
        .await?;

    // Resumed, the second row falls back (muted) in the new chapter: the
    // lift already paged it, so the fallback adds nothing.
    ctx.mute_interactions(&parent, true).await?;
    let resumed = ctx
        .post(&format!("/v1/instances/{parent}/resume"), json!({}))
        .await?;
    let successor = resumed["instance"]["instanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    ctx.report_running(ctx.node_p(), &successor).await?;
    ctx.node_p().append(&successor, session("idle"))?;
    let (_, reason, recipients) = ctx
        .wait_until("the second fallback settled", || async {
            let row = ctx.fallback("int_fb_lifted").ok()?;
            row.2.is_some().then_some(row)
        })
        .await?;
    assert_eq!(reason.as_deref(), Some("muted"));
    assert_eq!(recipients, Some(0), "the lift owns that page");
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(
        ctx.pushes_tagged(&ctx.phone, "interaction:int_fb_running")
            .len(),
        1
    );
    assert_eq!(
        ctx.pushes_tagged(&ctx.phone, "interaction:int_fb_lifted")
            .len(),
        1,
        "one page, through the lift, not two"
    );
    Ok(())
}
