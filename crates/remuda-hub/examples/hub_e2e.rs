//! In-process Hub plus a fake Node for Playwright live-hub e2e.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use remuda_hub::{DEFAULT_ENROLL_TOKEN_TTL_MINUTES, HubConfig, spawn};
use remuda_protocol::{HostId, InteractionId};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BOOTSTRAP: &str = "e2e-bootstrap-token";
const LISTEN: &str = "127.0.0.1:18787";
/// Fake Anthropic-Messages gateway for provider discovery.
const UPSTREAM_LISTEN: &str = "127.0.0.1:18788";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("remuda_hub=info".parse()?),
        )
        .init();

    let dir = tempfile::tempdir().context("e2e data dir")?;
    let origins = std::env::var("HUB_E2E_ORIGINS").unwrap_or_else(|_| {
        "http://127.0.0.1:4179,http://localhost:4179,http://127.0.0.1:4177".into()
    });
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.listen = std::env::var("HUB_E2E_LISTEN")
        .unwrap_or_else(|_| LISTEN.into())
        .parse::<SocketAddr>()
        .context("listen addr")?;
    config.web_root = std::env::var_os("REMUDA_WEB_ROOT").map(Into::into);
    config.bootstrap_token = BOOTSTRAP.into();
    config.cookie_secure = false;
    config.allowed_origins = origins
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // The Playwright hub suite mounts the login page dozens of times from a
    // single loopback IP and every mount starts a passkey ceremony, so use
    // effectively unlimited budgets. for_test keeps production defaults so
    // the 429 integration test still exercises the real buckets.
    config.auth_ip_burst = 1_000_000.0;
    config.auth_ip_refill_per_sec = 1_000.0;
    config.auth_global_burst = 1_000_000.0;
    config.auth_global_refill_per_sec = 1_000.0;
    // Local acceptance can attach the same fake engine to an isolated remuda
    // dev Hub/Node pair. CI still starts its own disposable real Hub here.
    let addr = config.listen;
    let hub = if std::env::var("HUB_E2E_EXTERNAL").as_deref() == Ok("1") {
        None
    } else {
        Some(spawn(config).await?)
    };
    let addr = hub.as_ref().map_or(addr, |hub| hub.addr);
    // D-018: the access code pairs devices; a Node enrolls with a one-shot
    // enroll token. In-process compositions mint one directly; attaching the
    // fake engine to an already-running Hub goes through the device HTTP path.
    let enroll = match &hub {
        Some(hub) => hub
            .mint_enroll_token(DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await
            .context("mint node enroll token")?,
        None => mint_enroll_via_device(addr, BOOTSTRAP)
            .await
            .context("mint node enroll token")?,
    };
    let host_id = HostId::new();
    let pending: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    let (ready_tx, ready_rx) = oneshot::channel();
    let node = tokio::spawn(fake_node(addr, enroll, host_id.clone(), pending, ready_tx));
    ready_rx.await.context("fake node hello")?;
    // A stand-in Anthropic-Messages gateway so provider discovery has a real
    // /v1/models to read without reaching any live endpoint.
    let upstream_listen =
        std::env::var("HUB_E2E_UPSTREAM_LISTEN").unwrap_or_else(|_| UPSTREAM_LISTEN.into());
    let upstream = tokio::net::TcpListener::bind(&upstream_listen)
        .await
        .context("fake upstream")?;
    let upstream_addr = upstream.local_addr()?;
    let upstream_task = tokio::spawn(fake_upstream(upstream));
    let line = json!({
        "hub": format!("http://{addr}"),
        "token": BOOTSTRAP,
        "hostId": host_id.as_id().as_str(),
        "upstream": format!("http://{upstream_addr}"),
    });
    println!("HUB_E2E_READY {line}");
    let _ = io::stdout().flush();
    tokio::signal::ctrl_c().await.ok();
    node.abort();
    upstream_task.abort();
    drop(hub);
    Ok(())
}

/// A catalog big enough to need the bulk controls, served under `/bulk/v1`.
///
/// Three prefix groups and 44 ids — the shape of a real gateway's listing,
/// where ticking one box at a time is the thing operators complained about.
/// Both surfaces answer with the same body, so the union is exactly these 44
/// in this order on every run.
fn bulk_catalog() -> String {
    let groups = [("cursor", 18), ("openai", 14), ("anthropic", 12)];
    let mut items: Vec<String> = Vec::new();
    for (prefix, count) in groups {
        for n in 1..=count {
            // Zero-padded so the ids sort and read the same everywhere.
            items.push(format!(r#"{{"id":"{prefix}/model-{n:02}"}}"#));
        }
    }
    format!(r#"{{"object":"list","data":[{}]}}"#, items.join(","))
}

/// Serve `/v1/models`, answering **differently per header** the way astergate
/// does: a plain Bearer GET returns the broad OpenAI-style list, while an
/// `anthropic-version` GET returns only the short `claude-*` subset. Discovery
/// must union both, so the checklist sees every id from either listing.
///
/// `/bulk/v1/models` is the same endpoint with a 44-id catalog behind it, for
/// the group and bulk-selection flows.
async fn fake_upstream(listener: tokio::net::TcpListener) {
    /// Plain `Authorization: Bearer` listing (OpenAI shape).
    const OPENAI_CATALOG: &str = r#"{"object":"list","data":[
        {"id":"e2e/auto","context_length":1048576},
        {"id":"e2e/fast","context_length":200000},
        {"id":"e2e/plain"},
        {"id":"cursor/e2e-wide"}
    ]}"#;
    /// `anthropic-version` listing (Anthropic shape): a short overlapping set
    /// plus one id the plain listing never mentions.
    const ANTHROPIC_CATALOG: &str = r#"{"data":[
        {"type":"model","id":"e2e/auto","display_name":"E2E Auto","context_window":1048576},
        {"type":"model","id":"e2e/fast","display_name":"E2E Fast","context_window":200000},
        {"type":"model","id":"claude-e2e-only","display_name":"Claude E2E"}
    ],"has_more":false}"#;
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let Ok(n) = stream.read(&mut buf).await else {
                return;
            };
            let head = String::from_utf8_lossy(&buf[..n]);
            let (code, body) = if head.starts_with("GET /bulk/v1/models") {
                // One catalog for both surfaces: the union is then exactly the
                // 44 ids, in one order, however the two probes interleave.
                (200, bulk_catalog())
            } else if head.starts_with("GET /v1/models") {
                // Header names are case-insensitive on the wire.
                if head.to_ascii_lowercase().contains("anthropic-version:") {
                    (200, ANTHROPIC_CATALOG.to_string())
                } else {
                    (200, OPENAI_CATALOG.to_string())
                }
            } else {
                (404, "{}".to_string())
            };
            let response = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
    }
}

async fn mint_enroll_via_device(addr: SocketAddr, bootstrap: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let login = client
        .post(format!("http://{addr}/v1/login"))
        .json(&json!({
            "bootstrapToken": bootstrap,
            "deviceName": "e2e-harness"
        }))
        .send()
        .await
        .context("login for enroll token")?;
    let cookie = login
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value.split(';').map(str::trim).find_map(|part| {
                part.strip_prefix("remuda_device=")
                    .filter(|token| !token.is_empty())
                    .map(|token| format!("remuda_device={token}"))
            })
        });
    login.error_for_status().context("login for enroll token")?;
    let cookie = cookie.ok_or_else(|| anyhow!("login cookie missing"))?;
    let minted: Value = client
        .post(format!("http://{addr}/v1/hosts/enroll-token"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .context("POST /v1/hosts/enroll-token")?
        .error_for_status()
        .context("POST /v1/hosts/enroll-token")?
        .json()
        .await?;
    minted
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("enroll token missing"))
}

async fn fake_node(
    addr: SocketAddr,
    enroll: String,
    host_id: HostId,
    pending: Arc<Mutex<HashMap<String, Value>>>,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let mut req = format!("ws://{addr}/v1/node")
        .into_client_request()
        .context("node ws")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {enroll}")
            .parse()
            .context("authorization header")?,
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    let host = host_id.as_id().as_str();
    let workspaces = json!([
        { "workspaceId": "wsp_e2e", "hostId": host, "root": "/tmp/remuda-e2e" },
        { "workspaceId": "wsp_e2e_second", "hostId": host, "root": "/tmp/remuda-e2e-second" },
        // G2 files-view synthetic scenarios (docs/design/files-view-contract.md §3.6).
        { "workspaceId": "wsp_g2_changes", "hostId": host, "root": "/tmp/remuda-g2/changes" },
        { "workspaceId": "wsp_g2_clean", "hostId": host, "root": "/tmp/remuda-g2/clean" },
        { "workspaceId": "wsp_g2_nogit", "hostId": host, "root": "/tmp/remuda-g2/nogit" },
        { "workspaceId": "wsp_g2_denied", "hostId": host, "root": "/tmp/remuda-g2/denied" },
        { "workspaceId": "wsp_g2_trunc", "hostId": host, "root": "/tmp/remuda-g2/trunc" },
        { "workspaceId": "wsp_g2_changed", "hostId": host, "root": "/tmp/remuda-g2/changed" }
    ]);
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0-e2e",
                "label": "e2e-fake-node",
                "host": {
                    "hostname": "e2e-fake-node.local",
                    "workspaceRevision": 1,
                    "workspaces": workspaces,
                    "labels": { "role": "e2e" },
                    // The shared Hub, slider and spaces scenarios create five instances.
                    "maxInstances": 8,
                    // Inventory only: this harness never launches herdr.
                    "herdr": { "version": "e2e-fake" },
                    "cli": [{
                        "kind": "claude",
                        "version": "2.1.268",
                        "absolutePath": "/usr/bin/claude",
                        "authState": "logged_in"
                    }]
                },
                // D-028 §5.1: the kind/driver matrix the web reads to default
                // New Session to the native shell-pty carrier. The Hub stores
                // this verbatim on the host view; nothing is hardcoded in the
                // web client.
                "capabilities": {
                    "driverInventory": [
                        {
                            "kind": "shell-pty",
                            "launchable": true,
                            "reasonCode": "fake-node-native-pty"
                        },
                        {
                            "kind": "claude-print",
                            "launchable": true,
                            "reasonCode": "fake-node-legacy"
                        },
                        {
                            "kind": "claude-pty",
                            "launchable": true,
                            "reasonCode": "fake-node-resume"
                        },
                        {
                            "kind": "generic-pty",
                            "launchable": true,
                            "reasonCode": "fake-node-legacy"
                        }
                    ]
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = match ws.next().await {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(other)) => anyhow::bail!("hello was {other}"),
        Some(Err(err)) => return Err(err.into()),
        None => anyhow::bail!("hello closed"),
    };
    let value: Value = serde_json::from_str(&hello)?;
    anyhow::ensure!(
        value["result"]["nodeToken"].as_str().is_some(),
        "enroll hello {value}"
    );
    let _ = ready.send(());
    let mut append_n = 0u64;
    // Minimal PTY harness for the QuickFind xterm e2e. It exists only while
    // this fake node is connected: a terminal session is registered on create,
    // tty.attach returns its stream + screen, and tty.write records raw bytes
    // (so the test can prove an Escape reached the process rather than being
    // swallowed by the browser) and echoes a visible marker back.
    let mut ttys: HashMap<String, TtyFake> = HashMap::new();
    // A failed Claude launch must not acquire a TTY through lazy attach.
    let mut claude_ptys = HashSet::new();
    while let Some(msg) = ws.next().await {
        let Ok(Message::Text(text)) = msg else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // Journal append acknowledgements share this socket with Hub requests.
        // Only this loop reads frames so concurrent RPCs are never discarded.
        if frame.get("method").is_none() {
            continue;
        }
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        let instance_id = params
            .get("instanceId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match method {
            "workspace.list" => {
                send_rpc_ok(
                    &mut ws,
                    id,
                    json!({"workspaceRevision": 1, "workspaces": workspaces}),
                )
                .await?;
            }
            "instance.create" | "instance.resume" => {
                let spec = params.get("spec").unwrap_or(&params);
                if spec.get("driver").and_then(Value::as_str) == Some("claude-pty") {
                    claude_ptys.insert(instance_id.clone());
                    // Model the real driver's launch prerequisite, so this
                    // e2e fails if resume omits either provider delivery field.
                    let has_overlay = spec.get("providerOverlay").is_some_and(Value::is_object);
                    let has_token = spec
                        .get("providerAuthToken")
                        .and_then(Value::as_str)
                        .is_some_and(|token| !token.is_empty());
                    if spec.get("delegation").and_then(Value::as_str) == Some("gateway")
                        && (!has_overlay || !has_token)
                    {
                        send_rpc_error(
                            &mut ws,
                            id,
                            "gateway delegation requires a settings overlay: fake Node did not receive providerOverlay and providerAuthToken",
                        )
                        .await?;
                        continue;
                    }
                    let session_id = spec
                        .get("resumeSessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                    ttys.entry(instance_id.clone()).or_insert_with(TtyFake::new);
                    append_n = append_instance_state(
                        &mut ws,
                        &instance_id,
                        append_n,
                        "ready",
                        Some(&session_id),
                    )
                    .await?;
                    send_rpc_ok(
                        &mut ws,
                        id,
                        json!({ "ok": true, "instanceId": instance_id }),
                    )
                    .await?;
                    continue;
                }
                if method == "instance.resume" {
                    send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                    continue;
                }
                let kind = params
                    .pointer("/spec/kind")
                    .or_else(|| params.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("claude");
                if kind == "terminal" {
                    // A raw terminal has no agent prompt, approval or journal
                    // turns; its surface is the PTY stream the QuickFind test
                    // attaches to.
                    ttys.entry(instance_id.clone()).or_insert_with(TtyFake::new);
                    send_rpc_ok(
                        &mut ws,
                        id,
                        json!({ "ok": true, "instanceId": instance_id }),
                    )
                    .await?;
                    continue;
                }
                let prompt = params
                    .pointer("/initialInput/text")
                    .or_else(|| params.get("prompt"))
                    .and_then(Value::as_str)
                    .unwrap_or("hello");
                let interaction_id = InteractionId::new();
                let card = fake_approval(
                    &instance_id,
                    host_id.as_id().as_str(),
                    interaction_id.as_id().as_str(),
                );
                pending
                    .lock()
                    .await
                    .insert(interaction_id.as_id().as_str().to_string(), card);
                append_n = append_journal(&mut ws, &instance_id, append_n, "user", prompt).await?;
                append_n = append_journal(
                    &mut ws,
                    &instance_id,
                    append_n,
                    "assistant",
                    &format!("echo: {prompt}"),
                )
                .await?;
                // A freshly launched native-PTY agent is mid-turn until the
                // web drives it; the approval keeps it blocked until answered.
                append_n = append_native_status(&mut ws, &instance_id, append_n, "working").await?;
                send_rpc_ok(
                    &mut ws,
                    id,
                    json!({ "ok": true, "instanceId": instance_id }),
                )
                .await?;
            }
            "instance.send" => {
                let prompt = params
                    .get("prompt")
                    .or_else(|| params.pointer("/text"))
                    .and_then(Value::as_str)
                    .unwrap_or("hello");
                append_n = append_journal(&mut ws, &instance_id, append_n, "user", prompt).await?;
                // D-027: echo the attachment metadata the Hub resolved, so the
                // e2e can prove staging reached the Node without a real agent.
                let attachments = params
                    .get("attachments")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(|item| item.get("mediaType").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let reply = if attachments.is_empty() {
                    format!("echo: {prompt}")
                } else {
                    format!("echo: {prompt} [attachments: {}]", attachments.join(","))
                };
                if prompt.starts_with("stream ") {
                    // D-028 §7: reply as an open/append chain so the web e2e
                    // sees text arrive mid-turn, the way `MessageDisplay`
                    // deltas do on a real agent-in-PTY session.
                    append_n =
                        append_stream_chunks(&mut ws, &instance_id, append_n, &reply).await?;
                } else {
                    append_n = append_journal(&mut ws, &instance_id, append_n, "assistant", &reply)
                        .await?;
                }
                // D-028 §6: a steer/queue send happens mid-turn; report the
                // native agent status so the web composer projects working.
                if params.get("mode").and_then(Value::as_str) == Some("new-turn") {
                    append_n =
                        append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                } else {
                    append_n =
                        append_native_status(&mut ws, &instance_id, append_n, "working").await?;
                }
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            "instance.cancel" => {
                // §5.3: the turn ends but the instance keeps running.
                append_n = append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            "instance.close" => {
                if claude_ptys.contains(&instance_id) {
                    ttys.remove(&instance_id);
                    append_n =
                        append_instance_state(&mut ws, &instance_id, append_n, "exited", None)
                            .await?;
                }
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            "interaction.list" => {
                let items: Vec<Value> = pending.lock().await.values().cloned().collect();
                send_rpc_ok(&mut ws, id, json!({ "items": items })).await?;
            }
            "interaction.answer" => {
                if let Some(iid) = params.get("interactionId").and_then(Value::as_str) {
                    pending.lock().await.remove(iid);
                }
                send_rpc_ok(
                    &mut ws,
                    id,
                    json!({ "ok": true, "state": "answer-committed" }),
                )
                .await?;
            }
            "workspace.scm.status" | "workspace.scm.diff" | "workspace.scm.file" => {
                let result = g2_scm_answer(method, &params);
                send_rpc_ok(&mut ws, id, result).await?;
            }
            "tty.attach" => {
                if claude_ptys.contains(&instance_id) && !ttys.contains_key(&instance_id) {
                    send_rpc_error(&mut ws, id, "instance has no TTY bridge").await?;
                    continue;
                }
                // The Hub asks for the live PTY stream when a follower opens
                // the terminal tab. Registering lazily keeps resume onto a
                // session created before this node connection behaved.
                let tty = ttys.entry(instance_id.clone()).or_insert_with(TtyFake::new);
                send_rpc_ok(
                    &mut ws,
                    id,
                    json!({
                        "ok": true,
                        "streamId": tty.stream_id,
                        "snapshotBase64": base64::engine::general_purpose::STANDARD
                            .encode(&tty.screen),
                        "availableFrom": "0",
                    }),
                )
                .await?;
            }
            "tty.write" => {
                // Raw keyboard bytes from the browser. Every byte is recorded
                // (the e2e reads the Escape back out of here indirectly via
                // the echoed marker) and printable input is echoed like a
                // cooked PTY would.
                let bytes = params
                    .get("dataBase64")
                    .and_then(Value::as_str)
                    .and_then(|raw| base64::engine::general_purpose::STANDARD.decode(raw).ok())
                    .unwrap_or_default();
                let tty = ttys.entry(instance_id.clone()).or_insert_with(TtyFake::new);
                tty.received.extend_from_slice(&bytes);
                let mut reply = Vec::new();
                for byte in &bytes {
                    // ESC is a control byte for the process, not display text.
                    if *byte != 0x1b {
                        reply.push(*byte);
                    }
                }
                if bytes.contains(&0x1b) {
                    reply.extend_from_slice(b"\r\nQUICKFIND_ESC_RECEIVED\r\n$ ");
                }
                if !reply.is_empty() {
                    tty.screen.extend_from_slice(&reply);
                    ws.send(Message::Text(
                        json!({
                            "jsonrpc": "2.0",
                            "method": "tty.frame",
                            "params": {
                                "instanceId": instance_id,
                                "streamId": tty.stream_id,
                                "dataBase64": base64::engine::general_purpose::STANDARD
                                    .encode(&reply),
                            },
                        })
                        .to_string()
                        .into(),
                    ))
                    .await?;
                }
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            "tty.resize" => {
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
            _ => {
                send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
            }
        }
    }
    Ok(())
}

/// Synthetic answers for the three read-only `workspace.scm.*` RPCs. Every body
/// is hand-authored fixture data (no real models, no real repository); the
/// workspace id selects the §3.6 availability scenario the Playwright spec
/// drives. `changed_first_status` implements row 9's stale-snapshot-then-refresh.
fn g2_scm_answer(method: &str, params: &Value) -> Value {
    let workspace_id = params
        .get("workspaceId")
        .and_then(Value::as_str)
        .unwrap_or("wsp_e2e");
    let observed_at = "2026-09-14T12:00:00.000Z";
    let head = "9c2f1a4b7d8e0f11223344556677889900aabbcc";
    let limits = json!({"maxEntries": 5000, "maxDiffBytes": 262144, "maxFileBytes": 1048576});
    let no_trunc = json!({"entries": false, "entriesOmitted": 0, "nonUtf8Omitted": 0,
                           "statusBytes": false});
    let status_envelope = |entries: Value, truncated: Value, root: &str| {
        json!({
            "workspaceId": workspace_id, "root": root, "scm": "git", "availability": "ok",
            "headOid": head,
            "branch": {"state": "known", "value": "feat/workbench-g2"},
            "observedAt": observed_at, "entries": entries, "limits": limits,
            "truncated": truncated, "ignoreRules": "git-default",
        })
    };

    if method == "workspace.scm.status" {
        return match workspace_id {
            "wsp_g2_clean" => status_envelope(json!([]), no_trunc, "/tmp/remuda-g2/clean"),
            "wsp_g2_nogit" => json!({
                "workspaceId": workspace_id, "root": "/tmp/remuda-g2/nogit", "scm": "git",
                "availability": "unsupported", "unsupportedReason": "not-a-git-repository",
                "headOid": null, "branch": {"state": "unknown", "reason": "unknown"},
                "observedAt": observed_at, "entries": [], "limits": limits,
                "truncated": no_trunc,
            }),
            "wsp_g2_denied" => json!({
                "workspaceId": workspace_id, "root": "/tmp/remuda-g2/denied", "scm": "git",
                "availability": "denied", "deniedReason": "permission-denied",
                "headOid": null, "branch": {"state": "unknown", "reason": "unknown"},
                "observedAt": observed_at, "entries": [], "limits": limits,
                "truncated": no_trunc,
            }),
            "wsp_g2_trunc" => status_envelope(
                json!([
                    {"path": "big.txt", "origPath": null, "xy": " M", "kind": "modified",
                     "sizeBytes": 2000000, "oldOid": "1111111111111111111111111111111111111111",
                     "newOid": "2222222222222222222222222222222222222222",
                     "digest": {"state": "unknown", "reason": "not-collected"}},
                    {"path": "many.txt", "origPath": null, "xy": "??", "kind": "untracked",
                     "sizeBytes": 300000, "oldOid": null, "newOid": null,
                     "digest": {"state": "unknown", "reason": "not-collected"}}
                ]),
                json!({"entries": true, "entriesOmitted": 12, "nonUtf8Omitted": 0,
                       "statusBytes": false}),
                "/tmp/remuda-g2/trunc",
            ),
            "wsp_g2_changed" => status_envelope(
                json!([
                    {"path": "src/edited.rs", "origPath": null, "xy": " M",
                     "kind": "modified", "sizeBytes": 64,
                     "oldOid": "3333333333333333333333333333333333333333",
                     "newOid": "4444444444444444444444444444444444444444",
                     "digest": {"state": "unknown", "reason": "not-collected"}}
                ]),
                no_trunc,
                "/tmp/remuda-g2/changed",
            ),
            _ => status_envelope(
                json!([
                    {"path": "src/main.rs", "origPath": null, "xy": " M", "kind": "modified",
                     "sizeBytes": 42,
                     "oldOid": "5555555555555555555555555555555555555555",
                     "newOid": "6666666666666666666666666666666666666666",
                     "digest": {"state": "unknown", "reason": "not-collected"}},
                    {"path": "notes/todo.md", "origPath": null, "xy": "??", "kind": "untracked",
                     "sizeBytes": 14, "oldOid": null, "newOid": null,
                     "digest": {"state": "unknown", "reason": "not-collected"}},
                    {"path": "assets/logo.bin", "origPath": null, "xy": " M",
                     "kind": "modified", "sizeBytes": 2048,
                     "oldOid": "7777777777777777777777777777777777777777",
                     "newOid": "8888888888888888888888888888888888888888",
                     "digest": {"state": "unknown", "reason": "not-collected"}}
                ]),
                no_trunc,
                "/tmp/remuda-e2e",
            ),
        };
    }

    if method == "workspace.scm.diff" {
        let path = params["paths"][0].as_str().unwrap_or("");
        let item = if workspace_id == "wsp_g2_trunc" {
            json!({"path": path, "patch": "diff --git a/many.txt b/many.txt\n@@\n+…\n",
                   "binary": false, "truncated": true, "bytesAvailable": 262144})
        } else if path == "assets/logo.bin" {
            json!({"path": path, "patch": null, "binary": true,
                   "truncated": false, "bytesAvailable": 0})
        } else {
            json!({"path": path,
                   "patch": "diff --git a/src/main.rs b/src/main.rs\nindex 5555555..6666666 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n+    println!(\"workbench g2\");\n }\n",
                   "binary": false, "truncated": false, "bytesAvailable": 96})
        };
        let diff_truncated = workspace_id == "wsp_g2_trunc";
        return json!({
            "workspaceId": workspace_id, "scm": "git", "availability": "ok",
            "headOid": head,
            "staged": params.get("staged").and_then(Value::as_bool).unwrap_or(false),
            "observedAt": observed_at, "items": [item], "limits": limits,
            "truncated": {"diffBytes": diff_truncated,
                          "bytesOmitted": if diff_truncated { 37856 } else { 0 }},
        });
    }

    // workspace.scm.file
    let path = params.get("path").and_then(Value::as_str).unwrap_or("");
    if workspace_id == "wsp_g2_trunc" {
        return json!({
            "workspaceId": workspace_id, "scm": "git", "availability": "ok",
            "path": path, "headOid": head, "observedAt": observed_at,
            "mediaType": "text/plain", "binary": false, "sizeBytes": 2000000,
            "digest": {"state": "unknown", "reason": "file-exceeds-inline-limit"},
            "content": null, "truncated": true, "limits": limits,
        });
    }
    json!({
        "workspaceId": workspace_id, "scm": "git", "availability": "ok",
        "path": path, "headOid": head, "observedAt": observed_at,
        "mediaType": "text/plain", "binary": false, "sizeBytes": 14,
        "digest": {"state": "known",
                   "value": "sha256:1111222233334444555566667777888899990000aaaabbbbccccddddeeeeffff0000"},
        "content": "# TODO\n- g2\n- ship\n",
        "truncated": false, "limits": limits,
    })
}

/// A script-free PTY double for the QuickFind xterm test.
///
/// It models only what that test needs: a stable stream id the Hub binds the
/// follower to, a one-screen buffer, and a raw byte log. Receiving an ESC byte
/// is acknowledged with an on-screen marker (`QUICKFIND_ESC_RECEIVED`) so a
/// browser test can prove the keystroke reached the process instead of being
/// eaten by a panel's key handler.
struct TtyFake {
    stream_id: String,
    screen: Vec<u8>,
    received: Vec<u8>,
}

impl TtyFake {
    fn new() -> Self {
        Self {
            stream_id: format!("tty_{}", uuid::Uuid::now_v7()),
            screen: b"fake-harness terminal\r\n$ ".to_vec(),
            received: Vec::new(),
        }
    }
}

async fn send_rpc_ok(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: Value,
    result: Value,
) -> Result<()> {
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
            .to_string()
            .into(),
    ))
    .await?;
    Ok(())
}

async fn send_rpc_error(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: Value,
    message: &str,
) -> Result<()> {
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } })
            .to_string()
            .into(),
    ))
    .await?;
    Ok(())
}

/// Confirm resource lifecycle and the native identity needed by Hub resume.
async fn append_instance_state(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    state: &str,
    session_id: Option<&str>,
) -> Result<u64> {
    let seq = n + 1;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": format!("j{seq}"), "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "event": {
                    "kind": "lifecycle",
                    "payload": {
                        "type": "entity", "entityType": "instance", "state": state,
                        "entity": { "nativeRef": { "sessionId": { "value": session_id } } }
                    }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(seq)
}

async fn append_journal(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    role: &str,
    text: &str,
) -> Result<u64> {
    let seq = n + 1;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": format!("j{seq}"),
            "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "event": {
                    "kind": "message",
                    "completeness": "structured",
                    "payload": { "role": role, "text": text }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(seq)
}

/// Append one assistant message as several `append` mutations.
///
/// This is the fake-node stand-in for a `MessageDisplay` delta chain: one node
/// id, contiguous revisions, `status: "streaming"` until the last chunk closes
/// it as `complete`. The web assembler must merge these into one growing
/// bubble rather than drawing a bubble per chunk.
async fn append_stream_chunks(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    text: &str,
) -> Result<u64> {
    // Split in the middle so both halves are non-empty and visibly partial.
    let split = text
        .char_indices()
        .nth(text.chars().count() / 2)
        .map_or(text.len(), |(i, _)| i);
    let chunks = [&text[..split], &text[split..]];
    let node = format!("stream-{n}");
    let mut seq = n;
    for (index, chunk) in chunks.iter().enumerate() {
        seq += 1;
        let last = index + 1 == chunks.len();
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": format!("j{seq}"),
                "method": "journal.append",
                "params": {
                    "instanceId": instance_id,
                    "event": {
                        "kind": "message",
                        "completeness": "structured",
                        "payload": {
                            "nodeId": node,
                            "messageId": node,
                            "role": "assistant",
                            "phase": "final",
                            "revision": (index + 1).to_string(),
                            "baseRevision": (index > 0).then(|| index.to_string()),
                            "operation": if index == 0 { "open" } else { "append" },
                            "status": if last { "complete" } else { "streaming" },
                            "blocks": [{ "type": "text", "text": chunk }],
                            "targetBlock": 0,
                            "origin": "human",
                        }
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    }
    Ok(seq)
}

async fn append_native_status(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    status: &str,
) -> Result<u64> {
    let seq = n + 1;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": format!("j{seq}"),
            "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "event": {
                    "kind": "lifecycle",
                    "payload": {
                        "type": "native",
                        "nativeName": "agent_status",
                        "status": status
                    }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(seq)
}

fn fake_approval(instance_id: &str, host_id: &str, interaction_id: &str) -> Value {
    json!({
        "id": interaction_id,
        "revision": "1",
        "createdAt": "2026-09-12T00:00:00.000Z",
        "updatedAt": "2026-09-12T00:00:00.000Z",
        "instanceId": instance_id,
        "runId": null,
        "hostId": host_id,
        "kind": "approval",
        "requestKey": {
            "native": { "type": "rpc", "valueType": "string", "value": "e2e-tool" },
            "processGeneration": "1",
            "runGeneration": "1",
            "connectionEpoch": host_id
        },
        "requestVersion": "1",
        "state": "pending",
        "blocking": true,
        "answerable": true,
        "carrier": "claude-control",
        "request": {
            "kind": "approval",
            "title": "Bash",
            "description": "echo e2e",
            "toolCallId": interaction_id,
            "actionRef": interaction_id,
            "options": [
                { "id": "allow-once", "label": "允许一次", "effect": "allow-once", "nativeValueRef": interaction_id },
                { "id": "deny", "label": "拒绝", "effect": "deny", "nativeValueRef": interaction_id }
            ],
            "requestedPermissionsRef": null,
            "inputDigest": "sha256:abababababababababababababababababababababababababababababababab"
        },
        "deadline": { "state": "unknown", "reason": "none", "evidenceEventIds": [] },
        "deadlineSource": "none",
        "answer": { "state": "not-applicable" },
        "delivery": "not-sent",
        "resolution": { "state": "not-applicable" }
    })
}
