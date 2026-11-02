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
