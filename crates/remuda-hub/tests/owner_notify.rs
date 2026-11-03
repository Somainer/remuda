//! D-060 §11.5 (cb-owner-notify): `POST /v1/owner/notify`, its claim rows,
//! the dedupe key, the push through the D-057 §8.3 outbox (its boot resend
//! under the same tag and per-device suppression), the notices the PWA lists
//! and acknowledges, and backup. Fake Node, fake push transport; every wait is
//! on durable state (no millisecond thresholds).
#![allow(missing_docs)]

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn, spawn_with_push};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// ─── fake push ──────────────────────────────────────────────────────────────

/// Records every Web Push POST by endpoint.
#[derive(Default)]
struct FakePush {
    hits: Mutex<Vec<(String, Vec<u8>)>>,
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

/// A fake Node that accepts every Hub request and appends journal events.
struct FakeNode {
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
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
    async fn connect(hub: &remuda_hub::RunningHub, host: &str) -> Result<Self> {
        let bearer = hub.mint_enroll_token(5).await?;
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let params = json!({ "hostId": host, "capabilities": {"lineageFence": true},
        "nodeEpoch": "epoch-1", "instanceStoreFound": true, "instances": [],
        "host": {
            "hostname": format!("{host}.notify"), "workspaces": [], "workspaceRevision": 0,
            "herdr": {"path": "/usr/bin/herdr"},
            "cli": [{"kind": "claude", "path": "/usr/bin/claude", "auth": "logged_in"}]
        }});
        node.send(Message::Text(
            json!({"jsonrpc":"2.0","id":"hello","method":"node.hello","params":params})
                .to_string()
                .into(),
        ))
        .await?;
        let hello: Value =
            serde_json::from_str(&node.next().await.context("hello")??.into_text()?)?;
        anyhow::ensure!(hello.get("result").is_some(), "hello failed: {hello}");
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
            task: Some(task),
        })
    }

    fn append(&self, instance_id: &str, event: Value) -> Result<()> {
        self.appends.send((instance_id.to_owned(), event))?;
        Ok(())
    }
}

// ─── harness ────────────────────────────────────────────────────────────────

struct Ctx {
    dir: tempfile::TempDir,
    data: PathBuf,
    bootstrap: String,
    hub: remuda_hub::RunningHub,
    push: Arc<FakePush>,
    http: reqwest::Client,
    /// The owner's phone: every owner call is made with its token.
    phone: Device,
    host: String,
    node: Option<FakeNode>,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let config = HubConfig::for_test(data.clone());
        let bootstrap = config.bootstrap_token.clone();
        let push = Arc::new(FakePush::default());
        let hub = spawn_with_push(config, push.clone()).await?;
        let http = reqwest::Client::new();
        let phone = Self::device(&hub, &http, "phone").await?;
        let host = HostId::new().as_id().as_str().to_owned();
        let node = FakeNode::connect(&hub, &host).await?;
        Ok(Self {
            dir,
            data,
            bootstrap,
            hub,
            push,
            http,
            phone,
            host,
            node: Some(node),
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

    fn db(&self) -> Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.data.join("hub.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
    }

    fn query_i64(&self, sql: &str, params: impl rusqlite::Params) -> Result<i64> {
        Ok(self.db()?.query_row(sql, params, |row| row.get(0))?)
    }

    /// One request as `token`, optionally narrowed to `instance` (the seat's
    /// CLI sends its instance id the same way). Returns status and body.
    async fn call(
        &self,
        method: reqwest::Method,
        token: &str,
        instance: Option<&str>,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(instance) = instance {
            request = request.header("x-remuda-instance-id", instance);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    async fn post_ok(&self, path: &str, body: Value) -> Result<Value> {
        let (status, body) = self
            .call(
                reqwest::Method::POST,
                &self.phone.token,
                None,
                path,
                Some(body),
            )
            .await?;
        anyhow::ensure!((200..300).contains(&status), "POST {path} {status}: {body}");
        Ok(body)
    }

    /// `remuda owner notify` as the seat `instance` (or the phone itself).
    async fn notify(&self, instance: Option<&str>, body: Value) -> Result<(u16, Value)> {
        self.call(
            reqwest::Method::POST,
            &self.phone.token,
            instance,
            "/v1/owner/notify",
            Some(body),
        )
        .await
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

    fn node(&self) -> &FakeNode {
        self.node.as_ref().expect("node")
    }

    async fn report_running(&self, id: &str) -> Result<()> {
        self.node().append(
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

    /// A running seat with `grants` (Main holds `address-owner`).
    async fn seat(&self, title: &str, grants: Value) -> Result<String> {
        let created = self
            .post_ok(
                "/v1/instances",
                json!({
                    "hostId": self.host, "kind": "claude", "driver": "claude-sdk",
                    "title": title, "grants": grants,
                    "permissionMode": "auto", "model": "claude-test-model",
                    "restart": {"onProcessLoss": true, "maxPerHour": 3},
                    "prompt": "seat brief",
                }),
            )
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("seat id")?
            .to_owned();
        self.report_running(&id).await?;
        Ok(id)
    }

    /// Decrypted payloads delivered to `device` and tagged `tag`.
    fn pushes_tagged(&self, device: &Device, tag: &str) -> Vec<Value> {
        self.push
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(url, _)| *url == device.endpoint)
            .map(|(_, body)| {
                let plain = ece::decrypt(&device.keys, &device.auth, body).expect("decrypt");
                serde_json::from_slice::<Value>(&plain).expect("payload")
            })
            .filter(|payload| payload["tag"] == tag)
            .collect()
    }

    async fn wait_pushes(&self, device: &Device, tag: &str, count: usize) -> Result<Vec<Value>> {
        self.wait_until(&format!("{count} push(es) {tag}"), || async {
            let pushes = self.pushes_tagged(device, tag);
            (pushes.len() >= count).then_some(pushes)
        })
        .await
    }

    /// Every outbox row delivered (a drive ran to completion).
    async fn outbox_drained(&self) -> Result<()> {
        self.hub.test_handle().test_drive_outbox().await;
        self.wait_until("the outbox drained", || async {
            let left = self
                .query_i64(
                    "SELECT COUNT(*) FROM push_outbox WHERE recipients IS NULL",
                    [],
                )
                .ok()?;
            (left == 0).then_some(())
        })
        .await
    }

    /// Stop the Hub as a crash leaves it, then start a new Hub on the same
    /// data with the same push fake.
    async fn crash_restart(&mut self) -> Result<()> {
        self.node.take();
        self.hub.shutdown_within(Duration::from_secs(10)).await;
        let mut config = HubConfig::for_test(self.data.clone());
        config.bootstrap_token = self.bootstrap.clone();
        self.hub = spawn_with_push(config, self.push.clone()).await?;
        Ok(())
    }
}

fn notice_id(body: &Value) -> Result<String> {
    Ok(body["notice"]["noticeId"]
        .as_str()
        .context("noticeId")?
        .to_owned())
}

// ─── the grant ──────────────────────────────────────────────────────────────

/// Only the address-owner seat and the owner's own device notify; a worker
/// without the grant and a Bot get 403 with a reason and a next step, and
/// nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_address_owner_or_the_owner_may_notify() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let worker = ctx.seat("Worker", json!(["dispatch"])).await?;
    let note = json!({"title": "Gate is green", "body": "All three lanes passed."});

    let (status, body) = ctx.notify(Some(&worker), note.clone()).await?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], "FORBIDDEN");
    assert_eq!(body["reason"], "grant-required");
    assert_eq!(body["next"]["grant"], "address-owner");
    assert!(body["next"]["hint"].as_str().is_some_and(|h| !h.is_empty()));

    let bot = ctx
        .call(
            reqwest::Method::POST,
            "unused",
            None,
            "/v1/login",
            Some(json!({ "bootstrapToken": ctx.hub.bootstrap_token,
                    "deviceName": "notify-bot", "deviceKind": "bot" })),
        )
        .await?
        .1["token"]
        .as_str()
        .context("bot token")?
        .to_owned();
    // A Bot is refused whether or not it names the seat.
    for instance in [None, Some(main.as_str())] {
        let (status, body) = ctx
            .call(
                reqwest::Method::POST,
                &bot,
                instance,
                "/v1/owner/notify",
                Some(note.clone()),
            )
            .await?;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["reason"], "address-owner-required");
        assert_eq!(body["next"]["action"], "ask-owner");
    }
    assert_eq!(
        ctx.query_i64("SELECT COUNT(*) FROM owner_notices", [])?,
        0,
        "a refusal stores nothing"
    );

    let (status, body) = ctx.notify(Some(&main), note.clone()).await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], true);
    assert_eq!(body["notice"]["chapterInstanceId"], json!(main));
    assert_eq!(body["notice"]["lineageId"], json!(main));
    assert_eq!(body["notice"]["origin"], "agent");
    let (status, body) = ctx.notify(None, note).await?;
    assert_eq!(status, 201, "the owner's own device: {body}");
    assert_eq!(body["notice"]["origin"], "human");
    assert_eq!(body["notice"]["chapterInstanceId"], Value::Null);

    // The notices routes are the owner's: an Agent is refused before them.
    let (status, _) = ctx
        .call(
            reqwest::Method::GET,
            &ctx.phone.token,
            Some(&main),
            "/v1/owner/notices",
            None,
        )
        .await?;
    assert_eq!(status, 403);
    Ok(())
}

/// r2 item 1: notice content and its acknowledgement are Human-only. A Bot
/// credential can neither list notices nor acknowledge one (so it cannot
/// remove a card from the owner's devices), whatever instance it names; an
/// Agent (a seat, or a Human token narrowed to one) is refused as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notices_are_read_and_acknowledged_only_by_a_human() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let (status, body) = ctx
        .notify(
            Some(&main),
            json!({"title": "Gate red", "body": "Lane 2 failed."}),
        )
        .await?;
    assert_eq!(status, 201, "{body}");
    let id = notice_id(&body)?;
    let bot = ctx
        .call(
            reqwest::Method::POST,
            "unused",
            None,
            "/v1/login",
            Some(json!({ "bootstrapToken": ctx.hub.bootstrap_token,
                    "deviceName": "notices-bot", "deviceKind": "bot" })),
        )
        .await?
        .1["token"]
        .as_str()
        .context("bot token")?
        .to_owned();
    let ack_path = format!("/v1/owner/notices/{id}/ack");
    for (token, instance) in [
        (bot.as_str(), None),
        (bot.as_str(), Some(main.as_str())),
        (ctx.phone.token.as_str(), Some(main.as_str())),
    ] {
        for (method, path) in [
            (reqwest::Method::GET, "/v1/owner/notices"),
            (reqwest::Method::GET, "/v1/owner/notices?unacked=1"),
            (reqwest::Method::POST, ack_path.as_str()),
        ] {
            let (status, body) = ctx
                .call(method.clone(), token, instance, path, None)
                .await?;
            assert_eq!(status, 403, "{method} {path} as {instance:?}: {body}");
            assert!(
                body["reason"] == "owner-only" || body["reason"] == "agent-route",
                "{body}"
            );
            assert_eq!(body["next"]["action"], "ask-owner", "{body}");
            assert!(
                !body.to_string().contains("Gate red"),
                "no notice text: {body}"
            );
        }
    }
    // The card is still there for the owner: nothing acknowledged it.
    let (status, page) = ctx
        .call(
            reqwest::Method::GET,
            &ctx.phone.token,
            None,
            "/v1/owner/notices?unacked=1",
            None,
        )
        .await?;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"][0]["noticeId"], json!(id));
    assert_eq!(page["items"][0]["ackedAt"], Value::Null);
    assert_eq!(
        ctx.query_i64(
            "SELECT COUNT(*) FROM owner_notices WHERE acked_at IS NOT NULL",
            []
        )?,
        0
    );
    Ok(())
}

/// A paused (fenced) Main chapter writes nothing (D-057 §7.3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_main_cannot_notify() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    ctx.post_ok(
        &format!("/v1/instances/{main}/commands"),
        json!({"operation": "instance.close", "payload": {}}),
    )
    .await?;
    ctx.wait_until("paused", || async {
        let state: Option<String> = ctx
            .db()
            .ok()?
            .query_row(
                "SELECT state FROM lineages WHERE lineage_id = ?1",
                [&main],
                |row| row.get(0),
            )
            .ok();
        (state.as_deref() == Some("paused")).then_some(())
    })
    .await?;
    let (status, body) = ctx
        .notify(Some(&main), json!({"title": "late", "body": "late"}))
        .await?;
    assert_eq!(status, 409, "{body}");
    assert_eq!(ctx.query_i64("SELECT COUNT(*) FROM owner_notices", [])?, 0);
    Ok(())
}

// ─── the secret lint ────────────────────────────────────────────────────────

/// A credential in the title, the body or the dedupe key is a 422 naming the
/// field, never echoing the text; nothing is stored or pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_is_refused_without_an_echo() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let secret = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
    for (field, body) in [
        (
            "title",
            json!({"title": format!("token {secret}"), "body": "b"}),
        ),
        (
            "body",
            json!({"title": "t", "body": format!("use {secret} now")}),
        ),
        (
            "dedupeKey",
            json!({"title": "t", "body": "b", "dedupeKey": secret}),
        ),
    ] {
        let (status, response) = ctx.notify(Some(&main), body).await?;
        assert_eq!(status, 422, "{response}");
        assert_eq!(response["code"], "OWNER_NOTIFY_SECRET_REJECTED");
        assert_eq!(response["field"], field);
        assert_eq!(response["next"]["action"], "fix-request");
        let text = response.to_string();
        assert!(!text.contains(&secret), "no echo: {text}");
        assert!(!text.contains("a1B2c3D4"), "no partial echo: {text}");
    }
    assert_eq!(ctx.query_i64("SELECT COUNT(*) FROM owner_notices", [])?, 0);
    assert_eq!(
        ctx.query_i64("SELECT COUNT(*) FROM push_outbox WHERE kind = 'notice'", [])?,
        0
    );
    // Bounds are a 400 with a next step.
    let (status, response) = ctx
        .notify(Some(&main), json!({"title": "x".repeat(121), "body": "b"}))
        .await?;
    assert_eq!(status, 400, "{response}");
    assert_eq!(response["reason"], "invalid-notice");
    Ok(())
}

// ─── claim, dedupe and push ─────────────────────────────────────────────────

/// The claim row exists before the push; a repeat with the same dedupe key
/// returns the original row and pushes nothing more; the push carries the
/// notice tag, title and landing, and is logged by category without its
/// text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedupe_returns_the_original_row_with_one_push() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let note = json!({"title": "Nightly gate red", "body": "Lane 2 failed: wire_golden.\nI paused landing.",
                      "dedupeKey": "gate:nightly:2026-11-03"});
    let (status, first) = ctx.notify(Some(&main), note.clone()).await?;
    assert_eq!(status, 201, "{first}");
    let id = notice_id(&first)?;
    let tag = format!("notice:{id}");
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(pushes[0]["title"], "Nightly gate red");
    assert_eq!(
        pushes[0]["body"],
        "Lane 2 failed: wire_golden. I paused landing."
    );
    assert_eq!(
        pushes[0]["data"]["url"],
        json!(format!("/sessions?focus={id}"))
    );
    ctx.outbox_drained().await?;

    let (status, again) = ctx
        .notify(
            Some(&main),
            json!({"title": "Nightly gate red (retry)", "body": "other words",
                   "dedupeKey": "gate:nightly:2026-11-03"}),
        )
        .await?;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["created"], false);
    assert_eq!(again["notice"], first["notice"], "the original row");
    ctx.outbox_drained().await?;
    assert_eq!(ctx.pushes_tagged(&ctx.phone, &tag).len(), 1, "one push");
    assert_eq!(ctx.query_i64("SELECT COUNT(*) FROM owner_notices", [])?, 1);
    assert_eq!(
        ctx.query_i64("SELECT COUNT(*) FROM push_outbox WHERE kind = 'notice'", [])?,
        1
    );
    // Audit: one row for the claim, without the text, bound to the device
    // the call authenticated as; the chapter is in the detail (r2 item 4).
    let (device, audit): (String, String) = ctx.db()?.query_row(
        "SELECT device_id, detail_json FROM audit_log
          WHERE action = 'owner.notify' AND subject = ?1",
        [&id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(
        device, ctx.phone.id,
        "the authenticating device, not the chapter"
    );
    let detail: Value = serde_json::from_str(&audit)?;
    assert_eq!(detail["chapterInstanceId"], json!(main));
    assert!(!audit.contains("Nightly"), "{audit}");
    assert!(!audit.contains("gate:nightly"), "{audit}");
    // The push log has the category and the tag, never the text.
    let logged = ctx.query_i64(
        "SELECT COUNT(*) FROM push_log WHERE category = 'notice' AND tag = ?1",
        [&tag],
    )?;
    assert_eq!(logged, 1);
    Ok(())
}

/// A crash after the claim committed and before any drive sent its push: the
/// claim row is there, nothing was pushed, and the restarted Hub sends it
/// once under the same tag. A crash between send and mark resends under the
/// same tag, which replaces the notification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_between_claim_and_push_resends_once_with_the_same_tag() -> Result<()> {
    let mut ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let held = ctx.hub.test_handle().test_hold_outbox_drive().await;
    let (status, body) = ctx
        .notify(
            Some(&main),
            json!({"title": "Reminder", "body": "Renew the cert today."}),
        )
        .await?;
    assert_eq!(status, 201, "{body}");
    let id = notice_id(&body)?;
    let tag = format!("notice:{id}");
    assert_eq!(
        ctx.query_i64(
            "SELECT COUNT(*) FROM owner_notices WHERE notice_id = ?1",
            [&id]
        )?,
        1,
        "the claim row committed"
    );
    assert_eq!(
        ctx.query_i64(
            "SELECT COUNT(*) FROM push_outbox WHERE tag_kind = 'notice' AND tag_id = ?1 \
             AND recipients IS NULL",
            [&id]
        )?,
        1,
        "its push is queued, unsent"
    );
    assert!(ctx.pushes_tagged(&ctx.phone, &tag).is_empty());
    // The old Hub never drives again: its drive stays held for the test.
    ctx.crash_restart().await?;
    let pushes = ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    assert_eq!(pushes[0]["tag"], json!(tag));
    ctx.outbox_drained().await?;
    ctx.hub.test_handle().test_drive_outbox().await;
    assert_eq!(ctx.pushes_tagged(&ctx.phone, &tag).len(), 1, "sent once");

    // Sent but not marked: the restart resends under the same tag (the
    // owner's own notice; it needs no Node).
    ctx.hub.test_handle().test_skip_outbox_marks(1);
    let (status, body) = ctx
        .notify(None, json!({"title": "Second", "body": "Mark lost."}))
        .await?;
    assert_eq!(status, 201, "{body}");
    let second = format!("notice:{}", notice_id(&body)?);
    ctx.wait_pushes(&ctx.phone, &second, 1).await?;
    ctx.crash_restart().await?;
    let both = ctx.wait_pushes(&ctx.phone, &second, 2).await?;
    assert_eq!(both[0]["tag"], both[1]["tag"]);
    ctx.outbox_drained().await?;
    drop(held);
    Ok(())
}

// ─── per device ─────────────────────────────────────────────────────────────

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
    let first = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await?
        .context("snapshot")??;
    anyhow::ensure!(matches!(first, Message::Text(_)), "{first:?}");
    Ok(ws)
}

/// A device that follows the writing chapter with its page visible is not
/// pushed (it is reading the card); every other device is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_device_suppression_applies() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let desktop = Ctx::device(&ctx.hub, &ctx.http, "desktop").await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let _desktop_ws = follow(&ctx, &desktop, &main).await?;
    let (status, body) = ctx
        .notify(Some(&main), json!({"title": "Done", "body": "Merged."}))
        .await?;
    assert_eq!(status, 201, "{body}");
    let tag = format!("notice:{}", notice_id(&body)?);
    ctx.wait_pushes(&ctx.phone, &tag, 1).await?;
    ctx.outbox_drained().await?;
    assert!(ctx.pushes_tagged(&desktop, &tag).is_empty());
    let suppressed: String = ctx.db()?.query_row(
        "SELECT suppressed_devices_json FROM push_log WHERE tag = ?1",
        [&tag],
        |row| row.get(0),
    )?;
    assert_eq!(suppressed, json!([desktop.id]).to_string());
    Ok(())
}

// ─── notices and the acknowledgement ────────────────────────────────────────

/// The owner lists unacknowledged notices, newest first, and per chapter;
/// an acknowledgement removes one from the unacknowledged list on every
/// device, is idempotent, and a notice acknowledged before its push went out
/// is not pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_lists_and_acknowledges_notices() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let tablet = Ctx::device(&ctx.hub, &ctx.http, "tablet").await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    let (_, a) = ctx
        .notify(Some(&main), json!({"title": "A", "body": "first"}))
        .await?;
    let (_, b) = ctx
        .notify(None, json!({"title": "B", "body": "second"}))
        .await?;
    let (a, b) = (notice_id(&a)?, notice_id(&b)?);
    ctx.outbox_drained().await?;
    let list = |token: String, query: &'static str| {
        let ctx = &ctx;
        async move {
            let (status, body) = ctx
                .call(
                    reqwest::Method::GET,
                    &token,
                    None,
                    &format!("/v1/owner/notices{query}"),
                    None,
                )
                .await?;
            anyhow::ensure!(status == 200, "{status} {body}");
            Ok::<Vec<String>, anyhow::Error>(
                body["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item["noticeId"].as_str().map(str::to_owned))
                    .collect(),
            )
        }
    };
    assert_eq!(
        list(ctx.phone.token.clone(), "?unacked=1").await?,
        vec![b.clone(), a.clone()]
    );
    let chapter_query: &'static str = Box::leak(format!("?instanceId={main}").into_boxed_str());
    assert_eq!(
        list(ctx.phone.token.clone(), chapter_query).await?,
        vec![a.clone()]
    );
    let (status, acked) = ctx
        .call(
            reqwest::Method::POST,
            &tablet.token,
            None,
            &format!("/v1/owner/notices/{a}/ack"),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{acked}");
    assert!(acked["notice"]["ackedAt"].is_string());
    // Every device: the phone's unacknowledged list no longer has it.
    assert_eq!(
        list(ctx.phone.token.clone(), "?unacked=1").await?,
        vec![b.clone()]
    );
    // The chapter's transcript still shows it, acknowledged.
    assert_eq!(
        list(ctx.phone.token.clone(), chapter_query).await?,
        vec![a.clone()]
    );
    let (status, again) = ctx
        .call(
            reqwest::Method::POST,
            &ctx.phone.token,
            None,
            &format!("/v1/owner/notices/{a}/ack"),
            None,
        )
        .await?;
    assert_eq!(status, 200);
    assert_eq!(again["notice"]["ackedAt"], acked["notice"]["ackedAt"]);
    let (status, _) = ctx
        .call(
            reqwest::Method::POST,
            &ctx.phone.token,
            None,
            "/v1/owner/notices/ntc_missing/ack",
            None,
        )
        .await?;
    assert_eq!(status, 404);

    // Acknowledged before its push went out: no push.
    let held = ctx.hub.test_handle().test_hold_outbox_drive().await;
    let (_, c) = ctx
        .notify(None, json!({"title": "C", "body": "third"}))
        .await?;
    let c = notice_id(&c)?;
    ctx.call(
        reqwest::Method::POST,
        &ctx.phone.token,
        None,
        &format!("/v1/owner/notices/{c}/ack"),
        None,
    )
    .await?;
    drop(held);
    ctx.outbox_drained().await?;
    assert!(
        ctx.pushes_tagged(&ctx.phone, &format!("notice:{c}"))
            .is_empty()
    );
    Ok(())
}

/// A notice records the turn it was written in, for the transcript card.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notice_records_the_turn_it_was_written_in() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let main = ctx.seat("Main", json!(["address-owner"])).await?;
    ctx.node().append(
        &main,
        json!({"kind":"lifecycle","payload":{"type":"native","topic":"turn","nativeName":"turn_started",
            "nativeId":{"state":"known","value":"session-1"},
            "status":{"state":"known","value":"working"},
            "relatedIds":{"nativeClientMessageId":"cmd_x"},"severity":"info","affectsCompletion":false}}),
    )?;
    let seq = ctx
        .wait_until("turn started", || async {
            ctx.query_i64(
                "SELECT seq FROM journal WHERE instance_id = ?1 \
                 AND json_extract(payload_json, '$.payload.nativeName') = 'turn_started'",
                [&main],
            )
            .ok()
        })
        .await?;
    let (_, body) = ctx
        .notify(Some(&main), json!({"title": "T", "body": "B"}))
        .await?;
    assert_eq!(body["notice"]["turnSeq"], json!(seq));
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

/// The claim rows ride `hub.sqlite`, so `scripts/hub-backup.sh` and a
/// restore carry them: the restored Hub lists the same notices and still
/// dedupes against them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notices_survive_backup_and_restore() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let (_, created) = ctx
        .notify(
            None,
            json!({"title": "Kept", "body": "Across a restore.", "dedupeKey": "kept-1"}),
        )
        .await?;
    ctx.outbox_drained().await?;
    let before = ctx
        .call(
            reqwest::Method::GET,
            &ctx.phone.token,
            None,
            "/v1/owner/notices",
            None,
        )
        .await?
        .1;
    let data = ctx.data.clone();
    let Ctx { dir, hub, node, .. } = ctx;
    drop(node);
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
    let restored = dir.path().join("restored");
    std::fs::create_dir_all(&restored)?;
    for member in manifest["members"].as_array().context("members")? {
        let rel = member["path"].as_str().context("path")?;
        let target = restored.join(rel);
        std::fs::create_dir_all(target.parent().context("parent")?)?;
        std::fs::copy(staged.join(rel), &target)?;
    }
    let hub = spawn(HubConfig::for_test(restored)).await?;
    let human = hub.mint_device_token("notify-restored").await?;
    let http = reqwest::Client::new();
    let after: Value = http
        .get(format!("http://{}/v1/owner/notices", hub.addr))
        .bearer_auth(&human)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(after, before, "the claim rows survive");
    let again: Value = http
        .post(format!("http://{}/v1/owner/notify", hub.addr))
        .bearer_auth(&human)
        .json(&json!({"title": "Kept", "body": "again", "dedupeKey": "kept-1"}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(again["created"], false);
    assert_eq!(again["notice"], created["notice"]);
    hub.shutdown().await;
    drop(dir);
    Ok(())
}
