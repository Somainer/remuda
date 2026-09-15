//! Coordinator batch 3 (co-supply) integration tests on fake nodes + a
//! fake-harness-emitted 429 journal line. No real models, no real remuda-node.
//!
//! Covers design §8.1 row 3:
//! * synthetic 429 text via journal.append → family window cooling,
//! * 529/overloaded does not cool,
//! * family window admits siblings while account-level parks all models,
//! * account `concurrency.max` enforced across TWO fake remote hosts,
//! * placement ledger rows carry `reasons[]` / `rejected[]`,
//! * dispatch with `taskSpec` selects the sibling and defers (429) on minClass.

use anyhow::{Context, Result, anyhow};
use futures::SinkExt;
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let token = hub.bootstrap_token.clone();
    Ok((hub, token, dir))
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, rest.to_string()))
}

fn cookie_from(head: &str) -> Option<String> {
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

/// Full HTTP call that also returns response headers (for login's cookie).
async fn http_head(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "supply-test" }).to_string();
    let (status, head, _) = http_head(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status}");
    cookie_from(&head).context("set-cookie")
}

/// Connected fake-node actor: owns the WS, answers Hub RPC requests, and
/// lets tests wait on specific JSON-RPC responses by id.
type AppendFn = std::sync::Arc<
    tokio::sync::Mutex<
        Box<dyn Fn(Value) -> futures::future::BoxFuture<'static, Result<Value>> + Send + Sync>,
    >,
>;

struct FakeNode {
    /// Host identity (`hst_…`).
    #[allow(dead_code)]
    host_id: String,
    /// Send a journal.append and await its result.
    append: AppendFn,
}

async fn enroll_fake_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    max_instances: u64,
) -> Result<std::sync::Arc<FakeNode>> {
    use futures::StreamExt;
    use tokio::sync::mpsc;
    let addr = hub.addr;
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut sink, mut stream) = ws.split();

    // Frames the test wants to send on the socket.
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    // JSON-RPC responses keyed by id, delivered to whoever is waiting.
    let (reply_tx, reply_rx) = mpsc::unbounded_channel::<(String, Value)>();
    let replies: std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, Value)>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(reply_rx));

    // Writer actor.
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });
    // Reader actor: auto-answer Hub requests, forward responses by id.
    let reader_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = stream.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            if frame.get("method").is_some() {
                let _ = reader_tx.send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ));
            } else if let Some(id) = id.as_str() {
                let _ = reply_tx.send((id.to_string(), frame));
            }
        }
    });

    // Hello handshake.
    tx.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": "fake-node",
                "host": { "hostname": format!("{host_id}.local"), "maxInstances": max_instances }
            }
        })
        .to_string()
        .into(),
    ))
    .map_err(|_| anyhow!("ws actor gone"))?;
    let hello = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        replies.lock().await.recv(),
    )
    .await
    .map_err(|_| anyhow!("hello timeout"))?
    .context("ws closed")?;
    anyhow::ensure!(
        hello.1["result"]["nodeToken"].as_str().is_some(),
        "{}",
        hello.1
    );

    let tx = std::sync::Arc::new(tokio::sync::Mutex::new(tx));
    let host_id_owned = host_id.to_string();
    let replies_for_append = replies.clone();
    let append: AppendFn =
        std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(move |body: Value| {
            let tx = tx.clone();
            let replies = replies_for_append.clone();
            Box::pin(async move {
                static CALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                let n = CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = format!("test-{n}");
                tx.lock()
                    .await
                    .send(Message::Text(
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": "journal.append",
                            "params": body
                        })
                        .to_string()
                        .into(),
                    ))
                    .map_err(|_| anyhow!("ws actor gone"))?;
                loop {
                    let (reply_id, frame) = tokio::time::timeout(
                        std::time::Duration::from_secs(8),
                        replies.lock().await.recv(),
                    )
                    .await
                    .map_err(|_| anyhow!("append timeout"))?
                    .context("ws closed")?;
                    if reply_id == id {
                        return Ok(frame["result"].clone());
                    }
                }
            })
        })));

    Ok(std::sync::Arc::new(FakeNode {
        host_id: host_id_owned,
        append,
    }))
}

/// Create a live instance row pinned to a profile/model and drive it to
/// `running` through the real lifecycle projection (no driver launch).
async fn seed_live_instance(
    hub: &remuda_hub::RunningHub,
    node: &std::sync::Arc<FakeNode>,
    host_id: &str,
    profile_id: &str,
    model: &str,
) -> Result<String> {
    let store = hub.store().expect("store");
    let spec = json!({
        "providerProfileId": profile_id,
        "model": model,
        "delegation": "gateway"
    });
    let instance = store
        .insert_instance(
            host_id.into(),
            None,
            "claude".into(),
            "claude-print".into(),
            Some("supply-seed".into()),
            spec,
        )
        .await?;
    // Rows are born 'requested' (excluded from live counts); a lifecycle
    // journal event is the real path that flips them to 'running'.
    node.append.lock().await(json!({
        "instanceId": instance.instance_id,
        "event": {
            "kind": "lifecycle",
            "payload": { "state": "running", "type": "entity" }
        }
    }))
    .await?;
    Ok(instance.instance_id)
}

fn relay_profile_with_two_models() -> Value {
    json!({
        "name": "relay",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "authToken": "sk-supply-test-secret-aaaa",
        "defaultGateway": true,
        "supply": { "priority": 20, "concurrency": { "max": 2 } },
        "models": [
            { "id": "gw/es1[1m]", "family": "es1", "role": "workhorse",
              "priority": 20, "fallback": ["gw/seed[1m]"] },
            { "id": "gw/seed[1m]", "family": "seed", "role": "workhorse",
              "priority": 18 }
        ]
    })
}

async fn create_relay(addr: std::net::SocketAddr, cookie: &str) -> Result<Value> {
    let body = relay_profile_with_two_models().to_string();
    let (status, rest) = http(
        addr,
        "POST",
        "/v1/providers",
        &[("Cookie", cookie)],
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "create relay {status} {rest}");
    Ok(serde_json::from_str(rest.trim())?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_endpoint_lists_revisioned_capabilities() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let (status, body) = http(
        hub.addr,
        "GET",
        "/v1/supply/catalog",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let value: Value = serde_json::from_str(body.trim())?;
    assert_eq!(value["revision"], remuda_hub::CATALOG_REVISION);
    let models = value["models"].as_array().unwrap();
    let opus = models
        .iter()
        .find(|m| m["id"] == "claude-opus-5")
        .context("opus row")?;
    assert_eq!(opus["family"], "opus");
    assert_eq!(opus["class"], "frontier");
    assert_eq!(opus["supports"]["vision"], true);
    assert!(models.iter().any(|m| m["id"] == "gpt-5-mini"));
    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn textual_429_via_fake_node_cools_family_and_529_does_not() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let relay = create_relay(hub.addr, &cookie).await?;
    let profile_id = relay["id"].as_str().unwrap();

    // Seed a live instance on the profile so the journal event resolves it.
    let host_id = HostId::new();
    let host = host_id.as_id().to_string();
    let node = enroll_fake_node(&hub, &host, 8).await?;
    let instance_id = seed_live_instance(&hub, &node, &host, profile_id, "gw/es1[1m]").await?;

    // The exact 09-14 text: Request rejected (429) {"error_code":-2001}.
    let append = json!({
        "instanceId": instance_id,
        "event": {
            "kind": "message",
            "payload": {
                "role": "assistant",
                "text": "Request rejected (429) {\"error_code\":-2001}"
            }
        }
    });
    let result = node.append.lock().await(append).await?;
    assert_eq!(result["replayed"], false);

    let (status, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers/{profile_id}/supply"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let supply: Value = serde_json::from_str(body.trim())?;
    let windows = supply["supply"]["windows"].as_array().unwrap();
    let model_window = windows
        .iter()
        .find(|w| w["id"] == "model")
        .context("model window after 429")?;
    assert_eq!(model_window["appliesTo"][0], "es1");
    assert_eq!(model_window["usedPercent"], 100.0);
    assert_eq!(model_window["source"], "observed");
    assert!(model_window["cooldownUntil"].as_i64().is_some());
    assert_eq!(supply["supply"]["state"], "degraded");

    // A 529 textual event on the same model changes nothing.
    let mut append_529 = json!({
        "instanceId": instance_id,
        "event": {
            "kind": "message",
            "payload": { "role": "assistant", "text": "529 overloaded, retry upstream" }
        }
    });
    append_529["event"]["payload"]["text2"] = json!("ignored");
    node.append.lock().await(append_529).await?;
    let (_, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers/{profile_id}/supply"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    let supply: Value = serde_json::from_str(body.trim())?;
    let windows = supply["supply"]["windows"].as_array().unwrap();
    let still_cooling = windows
        .iter()
        .any(|w| w["id"] == "model" && w["cooldownUntil"].is_number());
    assert!(
        still_cooling,
        "429 cooldown intact; 529 must not extend windows"
    );
    assert!(
        supply["supply"]["lastError"]
            .as_str()
            .unwrap_or_default()
            .contains("429"),
        "lastError should still be the 429, got {}",
        supply["supply"]["lastError"]
    );

    // Audit log records supply.cooldown.
    let audits = hub
        .store()
        .unwrap()
        .audit_for(profile_id.to_string())
        .await?;
    assert!(
        audits.iter().any(|row| row["action"] == "supply.cooldown"),
        "audit log missing supply.cooldown: {audits:?}"
    );
    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn family_window_admits_sibling_account_window_parks_all() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let _h1 = enroll_fake_node(&hub, "hst_sup_a_1_111111111111111111", 8).await?;
    let _h2 = enroll_fake_node(&hub, "hst_sup_a_2_222222222222222222", 8).await?;
    let relay = create_relay(hub.addr, &cookie).await?;
    let profile_id = relay["id"].as_str().unwrap();

    // Structured family window (es1 full). Sibling seed must still win.
    let structured = json!({
        "type": "structured",
        "windows": [
            {
                "id": "model",
                "appliesTo": ["es1"],
                "windowDurationMins": 300,
                "usedPercent": 100,
                "resetsAt": 9_999_999_999_i64
            }
        ]
    })
    .to_string();
    let (status, _) = http(
        hub.addr,
        "POST",
        &format!("/v1/providers/{profile_id}/supply/events"),
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&structured),
    )
    .await?;
    assert_eq!(status, 200);

    let probe = json!({ "taskSpec": { "class": "implement" } }).to_string();
    let (status, body) = http(
        hub.addr,
        "POST",
        "/v1/supply/resolve",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&probe),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let decision: Value = serde_json::from_str(body.trim())?;
    assert_eq!(decision["chosen"]["profileId"], profile_id);
    assert_eq!(decision["chosen"]["modelId"], "gw/seed[1m]");
    assert_eq!(decision["chosen"]["family"], "seed");
    assert!(decision["deferred"] == false);
    assert!(
        decision["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["modelId"] == "gw/es1[1m]"
                && r["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x.as_str().unwrap_or_default().contains("cooling"))),
        "es1 rejected with a cooling reason: {}",
        decision["rejected"]
    );

    // Now an account-level weekly window: everything parks.
    let account = json!({
        "type": "structured",
        "windows": [
            {
                "id": "weekly",
                "appliesTo": ["*"],
                "windowDurationMins": 10080,
                "usedPercent": 100,
                "resetsAt": 9_999_999_999_i64
            }
        ]
    })
    .to_string();
    let (status2, body2) = http(
        hub.addr,
        "POST",
        &format!("/v1/providers/{profile_id}/supply/events"),
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&account),
    )
    .await?;
    assert_eq!(status2, 200, "{body2}");
    let supply: Value = serde_json::from_str(body2.trim())?;
    assert_eq!(supply["supply"]["state"], "cooling");
    let (_, body) = http(
        hub.addr,
        "POST",
        "/v1/supply/resolve",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&probe),
    )
    .await?;
    let decision: Value = serde_json::from_str(body.trim())?;
    assert!(decision["deferred"].as_bool().unwrap_or(false));
    assert!(decision["chosen"].is_null());
    assert!(decision["deferredUntil"].is_number());
    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_concurrency_max_enforced_across_two_fake_hosts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let h1 = HostId::new().as_id().to_string();
    let h2 = HostId::new().as_id().to_string();
    let n1 = enroll_fake_node(&hub, &h1, 8).await?;
    let n2 = enroll_fake_node(&hub, &h2, 8).await?;
    let relay = create_relay(hub.addr, &cookie).await?;
    let profile_id = relay["id"].as_str().unwrap();

    // Two live uses split across the two hosts meet concurrency.max=2.
    seed_live_instance(&hub, &n1, &h1, profile_id, "gw/es1[1m]").await?;
    seed_live_instance(&hub, &n2, &h2, profile_id, "gw/seed[1m]").await?;

    let probe = json!({ "taskSpec": { "class": "implement" } }).to_string();
    let (_, body) = http(
        hub.addr,
        "POST",
        "/v1/supply/resolve",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&probe),
    )
    .await?;
    let decision: Value = serde_json::from_str(body.trim())?;
    assert!(decision["deferred"].as_bool().unwrap_or(false), "{body}");
    let reasons = decision["rejected"][0]["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap_or_default().contains("concurrency.max 2")),
        "{body}"
    );
    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_task_spec_defers_below_min_class_and_writes_ledger() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host = HostId::new().as_id().to_string();
    let _node = enroll_fake_node(&hub, &host, 8).await?;

    // A profile offering only a cheap model.
    let cheap = json!({
        "name": "cheap-only",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "authToken": "sk-supply-cheap-secret-bbbb",
        "models": [{ "id": "claude-haiku-4-5", "role": "cheap" }],
        "supply": { "priority": 10, "windows": [
            { "id": "primary", "appliesTo": ["*"], "windowDurationMins": 300,
              "usedPercent": 0, "source": "observed" }
        ] }
    })
    .to_string();
    let (status, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&cheap),
    )
    .await?;
    assert_eq!(status, 200, "{rest}");
    let profile: Value = serde_json::from_str(rest.trim())?;
    let profile_id = profile["id"].as_str().unwrap();

    // A workhorse dispatch admits (haiku is below workhorse too! so use a
    // workhorse profile as well) — add a sonnet row to the same profile.
    // Simpler: defer case with minClass frontier.
    let body = json!({
        "kind": "claude",
        "driver": "claude-print",
        "taskSpec": { "minClass": "frontier" }
    })
    .to_string();
    let (status, rest) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 429, "expected SUPPLY_DEFERRED, got {status} {rest}");
    let error: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(error["code"], "SUPPLY_DEFERRED");
    assert!(error["deferred"].as_bool().unwrap_or(false));
    assert!(
        error["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["reasons"].as_array().unwrap().iter().any(|x| x
                .as_str()
                .unwrap_or_default()
                .contains("below minClass frontier"))),
        "{rest}"
    );

    // Successful dispatch writes a placement ledger row with reasons[].
    let body = json!({
        "kind": "claude",
        "driver": "claude-print",
        "model": "gw/work[1m]",
        "providerProfileId": profile_id,
        "delegation": "gateway",
        "taskSpec": { "minClass": "cheap" }
    })
    .to_string();
    // The cheap profile's catalog doesn't know gw/work, so declare a workhorse
    // model on it first through a models PATCH.
    let patch = json!({
        "models": [
            { "id": "claude-haiku-4-5", "role": "cheap", "priority": 1 },
            { "id": "gw/work[1m]", "family": "work", "role": "workhorse", "priority": 5 }
        ]
    })
    .to_string();
    let (pstatus, prest) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{profile_id}"),
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&patch),
    )
    .await?;
    assert_eq!(pstatus, 200, "{prest}");

    let (status, rest) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 200, "dispatch {status} {rest}");
    let created: Value = serde_json::from_str(rest.trim())?;
    let instance_id = created["instance"]["instanceId"].as_str().unwrap();

    let rows = hub
        .store()
        .unwrap()
        .list_placement_ledger(10)
        .await
        .map_err(|err| anyhow!("ledger: {err}"))?;
    let row = rows
        .iter()
        .find(|r| r["instanceId"] == instance_id)
        .context("placement ledger row for dispatch")?;
    assert_eq!(row["chosen"]["modelId"], "gw/work[1m]");
    assert!(
        row["reasons"].as_array().is_some_and(|r| !r.is_empty()),
        "ledger reasons[] missing: {row}"
    );
    assert!(row["ranked"].is_array());
    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_events_flow_through_journal_into_aggregation() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host = HostId::new().as_id().to_string();
    let node = enroll_fake_node(&hub, &host, 8).await?;
    let relay = create_relay(hub.addr, &cookie).await?;
    let profile_id = relay["id"].as_str().unwrap();
    let instance_id = seed_live_instance(&hub, &node, &host, profile_id, "gw/es1[1m]").await?;

    let usage = json!({
        "instanceId": instance_id,
        "event": {
            "schemaVersion": 1,
            "eventId": "evt_01993ab0-0000-7000-8000-000000000099",
            "journalId": "obj_01993ab0-0000-7000-8000-000000000001",
            "instanceId": instance_id,
            "hostId": host,
            "processGeneration": "1",
            "seq": "1",
            "observedAt": "2026-09-15T10:00:00.000Z",
            "kind": "usage",
            "completeness": "structured",
            "payload": {
                "usageId": "obj_01993ab0-0000-7000-8000-000000000002",
                "scope": "turn",
                "scopeId": "01993ab0-0000-7000-8000-000000000003",
                "mode": "snapshot",
                "metricRevision": "1",
                "inputAccounting": "uncached",
                "accounting": "estimated",
                "inputTokens": { "state": "known", "value": "1000" },
                "outputTokens": { "state": "known", "value": "500" },
                "reasoningTokens": { "state": "unknown", "reason": "x", "evidenceEventIds": [] },
                "cacheReadTokens": { "state": "unknown", "reason": "x", "evidenceEventIds": [] },
                "cacheWriteTokens": { "state": "unknown", "reason": "x", "evidenceEventIds": [] },
                "totalTokens": { "state": "known", "value": "1500" },
                "cost": { "state": "known", "value": { "amount": "0.03", "currency": "USD" } }
            }
        }
    });
    node.append.lock().await(usage).await?;

    let (_, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}/usage"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    let agg: Value = serde_json::from_str(body.trim())?;
    assert_eq!(agg["events"], 1);
    assert_eq!(agg["totalTokens"], 1500);
    assert_eq!(agg["estimatedUsd"], 0.03);

    let (_, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers/{profile_id}/usage?budgetMaxUsd=0.02"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    let usage: Value = serde_json::from_str(body.trim())?;
    let es1 = usage["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "gw/es1[1m]")
        .context("es1 usage row")?;
    assert_eq!(es1["budgetStatus"], "stop"); // 0.03 >= 0.02 * 1.15
    hub.shutdown().await;
    Ok(())
}
