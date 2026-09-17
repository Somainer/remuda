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
use std::time::Duration;
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
    // D-027b: the browser suite runs against a deliberately small per-file
    // ceiling so the size-cap assertion uploads only ~100 KiB instead of
    // pushing 26 MiB through a remote browser. Production defaults stay at
    // 25 MiB (config::DEFAULT_ATTACHMENT_MAX_BYTES); this is an e2e fixture.
    // Override with HUB_E2E_ATTACHMENT_MAX_BYTES when a spec needs more room.
    config.attachment_max_bytes = match std::env::var("HUB_E2E_ATTACHMENT_MAX_BYTES") {
        Ok(limit) => limit
            .parse()
            .with_context(|| format!("HUB_E2E_ATTACHMENT_MAX_BYTES={limit}"))?,
        Err(_) => 64 * 1024,
    };
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
                    }, {
                        "kind": "codex",
                        "version": "0.154.0-e2e-fake",
                        "absolutePath": "/usr/bin/codex",
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
    // The enroll token is single-use; object pulls need the durable host token
    // the hello exchanges it for (the real Node does exactly this).
    let durable_token: String = value["result"]["nodeToken"]
        .as_str()
        .unwrap_or(&enroll)
        .to_owned();
    let _ = ready.send(());
    let mut append_n = 0u64;
    // Minimal PTY harness for the xterm e2e specs. A terminal session is
    // registered on create, tty.attach returns its stable per-instance stream
    // + screen, and tty.write records raw bytes (QuickFind proves an Escape
    // reached the process) and echoes printable input; CR additionally submits
    // a commandId-less journal user node (C2 native-typing correlation).
    let mut ttys: HashMap<String, TtyFake> = HashMap::new();
    // A failed Claude launch must not acquire a TTY through lazy attach.
    let mut claude_ptys = HashSet::new();
    let mut instance_kinds: HashMap<String, String> = HashMap::new();
    // Scripted terminal answers: a spawned timer sends (instance, iid, answers)
    // back into this loop so journal appends stay single-writer.
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<(String, String, Value)>(8);
    // Frames read while waiting for a journal.append ack (they may be stale
    // append responses or — importantly — unrelated Hub RPCs such as the
    // web poll's interaction.list). Never drop RPCs: reprocess them as soon
    // as the current handler returns.
    let mut frame_queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    loop {
        tokio::select! {
            biased;
            Some((closed_instance, closed_iid, terminal_answers)) = close_rx.recv() => {
                let Some(card) = pending.lock().await.remove(&closed_iid) else {
                    continue;
                };
                // A device answer may have won the first-answer race; the
                // timer then finds nothing. When the terminal wins, journal:
                // the interaction entity resolved (source terminal, chosen
                // labels), an assistant turn carrying the labels, then idle.
                let resolution_seq = append_terminal_resolution(
                    &mut ws,
                    &closed_instance,
                    append_n,
                    &card,
                    &terminal_answers,
                )
                .await?;
                wait_frame_ack(&mut ws, &mut frame_queue, &format!("j{resolution_seq}")).await?;
                append_n = resolution_seq;
                let assistant_seq = append_journal(
                    &mut ws,
                    &closed_instance,
                    append_n,
                    "assistant",
                    &format!(
                        "AskUserQuestion answered in terminal: {}",
                        question_answer_summary(&terminal_answers)
                    ),
                )
                .await?;
                wait_frame_ack(&mut ws, &mut frame_queue, &format!("j{assistant_seq}")).await?;
                append_n = assistant_seq;
                let idle_seq =
                    append_native_status(&mut ws, &closed_instance, append_n, "idle").await?;
                wait_frame_ack(&mut ws, &mut frame_queue, &format!("j{idle_seq}")).await?;
                append_n = idle_seq;
                continue;
            }
            msg = ws.next() => {
            let Some(msg) = msg else { break };
        let Ok(Message::Text(text)) = msg else {
            continue;
        };
        frame_queue.push_back(text.to_string());
        }
        }
        // Process everything that arrived, including frames stashed behind an
        // append ack wait.
        while let Some(text) = frame_queue.pop_front() {
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
                    if let Some(kind) = spec.get("kind").and_then(Value::as_str) {
                        instance_kinds.insert(instance_id.clone(), kind.to_owned());
                    }
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
                        // §9.1 model-sync: the driver's launch snapshot carries the
                        // gateway-discovered catalog and the launch model so the
                        // picker shows the real list immediately.
                        let launch_model = spec
                            .get("modelId")
                            .or_else(|| spec.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or("e2e/auto")
                            .to_string();
                        append_n = append_event(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "model",
                            json!({
                                "requested": launch_model,
                                "effective": {
                                    "id": launch_model,
                                    "source": "launch",
                                    "observedAt": "2026-09-14T12:00:00.000Z"
                                },
                                "catalog": {
                                    "models": [
                                        "e2e/auto",
                                        "e2e/fast",
                                        "e2e/plain",
                                        "claude-e2e-only"
                                    ],
                                    "source": "gateway-discovery",
                                    "observedAt": "2026-09-14T12:00:00.000Z"
                                }
                            }),
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
                    // A prompt naming the hook path raises the D-028 §4.4 tier A
                    // card instead: harness-hook carrier, the real tool input as
                    // its description, and an always-allow option built from the
                    // permission_suggestion the harness offered.
                    let card = if prompt.contains("ask-question") {
                        fake_hook_question(
                            &instance_id,
                            host_id.as_id().as_str(),
                            interaction_id.as_id().as_str(),
                        )
                    } else if prompt.contains("hook-approval") {
                        fake_hook_approval(
                            &instance_id,
                            host_id.as_id().as_str(),
                            interaction_id.as_id().as_str(),
                        )
                    } else {
                        fake_approval(
                            &instance_id,
                            host_id.as_id().as_str(),
                            interaction_id.as_id().as_str(),
                        )
                    };
                    let terminal_answer = prompt.contains("ask-question-terminal");
                    pending
                        .lock()
                        .await
                        .insert(interaction_id.as_id().as_str().to_string(), card.clone());
                    if terminal_answer {
                        // Model the human answering in the agent's own TUI: after a
                        // beat the harness closes the dialog itself (PostToolUse
                        // with answers), without any interaction.answer RPC.
                        let tx = close_tx.clone();
                        let iid = interaction_id.as_id().as_str().to_string();
                        let terminal_instance = instance_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(1200)).await;
                            let _ = tx
                            .send((
                                terminal_instance,
                                iid,
                                json!({
                                    "q0": { "optionIds": ["继续排查 remuda 环境"], "text": null },
                                    "q1": { "optionIds": ["保存端口"], "text": null }
                                }),
                            ))
                            .await;
                        });
                    }
                    // C2: the create prompt is a command, so its user observation
                    // carries the commandId the Hub forwards in params.
                    let command_id = params.get("commandId").and_then(Value::as_str);
                    append_n =
                        append_command_user(&mut ws, &instance_id, append_n, prompt, command_id)
                            .await?;
                    // r-ux-comment: a fenced block to exercise 评论.
                    if let Some(reply) = code_comment_reply(prompt) {
                        append_n =
                            append_journal(&mut ws, &instance_id, append_n, "assistant", &reply)
                                .await?;
                    } else {
                        append_n = append_journal(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "assistant",
                            &format!("echo: {prompt}"),
                        )
                        .await?;
                    }
                    // A freshly launched native-PTY agent is mid-turn until the
                    // web drives it; the approval keeps it blocked until answered.
                    append_n =
                        append_native_status(&mut ws, &instance_id, append_n, "working").await?;
                    // §9.1 model-sync: the launch snapshot carries the
                    // gateway-discovered catalog and current model for any claude
                    // carrier (claude-pty emits in its own branch above; shell-pty
                    // and print land here).
                    if kind == "claude" {
                        let launch_model = spec
                            .get("modelId")
                            .or_else(|| spec.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or("e2e/auto")
                            .to_string();
                        append_n = append_event(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "model",
                            json!({
                                "requested": launch_model,
                                "effective": {
                                    "id": launch_model,
                                    "source": "launch",
                                    "observedAt": "2026-09-14T12:00:00.000Z"
                                },
                                "catalog": {
                                    "models": [
                                        "e2e/auto",
                                        "e2e/fast",
                                        "e2e/plain",
                                        "claude-e2e-only"
                                    ],
                                    "source": "gateway-discovery",
                                    "observedAt": "2026-09-14T12:00:00.000Z"
                                }
                            }),
                        )
                        .await?;
                    }
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
                    let command_id = params.get("commandId").and_then(Value::as_str);
                    // §9.1: a terminal-side `/effort <level>` typed in the PTY is
                    // observed as a hand-typed slash command — the fake node emits
                    // the matching effort observation attributed to `slash`, and
                    // nothing calls instance.configure back (no ping-pong).
                    if let Some(word) = prompt.strip_prefix("/effort:") {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        let (tier, ultra) = if word == "ultracode" {
                            ("xhigh", serde_json::Value::Bool(true))
                        } else {
                            (word, serde_json::Value::Bool(false))
                        };
                        append_n = append_event(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "effort",
                            json!({
                                "effective": {
                                    "name": tier,
                                    "ultracode": ultra,
                                    "source": "slash",
                                    "observedAt": monotonic_effort_observed_at()
                                },
                                "raw": tier
                            }),
                        )
                        .await?;
                        continue;
                    }
                    // context-usage-1: a `usage:in,out,cacheRead,cacheWrite`
                    // prompt appends one full protocol usage observation (each
                    // position `-` = the channel was not reported, exercising the
                    // Hub rollup's unknown-not-zero rule), then ends the turn.
                    if let Some(usage) = scripted_usage(prompt) {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_n = append_command_user(
                            &mut ws,
                            &instance_id,
                            append_n,
                            prompt,
                            command_id,
                        )
                        .await?;
                        append_n =
                            append_event(&mut ws, &instance_id, append_n, "usage", usage).await?;
                        append_n = append_journal(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "assistant",
                            &format!("usage recorded: {prompt}"),
                        )
                        .await?;
                        append_n =
                            append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                        continue;
                    }
                    // §9.1 model-sync: a terminal-side `/model <id>` typed in the
                    // PTY emits the matching model observation attributed to
                    // `slash` (resolved id verbatim), no configure ping-pong.
                    if let Some(model) = prompt.strip_prefix("/model:") {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_n = append_event(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "model",
                            json!({
                                "effective": {
                                    "id": model,
                                    "source": "slash",
                                    "observedAt": "2026-09-14T12:00:00.000Z"
                                },
                                "raw": model
                            }),
                        )
                        .await?;
                        continue;
                    }
                    // r-ux-comment: reply with a fenced code block so the browser
                    // spec can exercise the 评论 quote action. The prompt is also
                    // echoed verbatim below, proving the expanded quote arrived.
                    if let Some(reply) = code_comment_reply(prompt) {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_n = append_command_user(
                            &mut ws,
                            &instance_id,
                            append_n,
                            prompt,
                            command_id,
                        )
                        .await?;
                        append_n =
                            append_journal(&mut ws, &instance_id, append_n, "assistant", &reply)
                                .await?;
                        append_n =
                            append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                        continue;
                    }
                    // r-ux-w: synthetic workflow timeline-card scenarios take the
                    // short path (their own scripted journal sequence).
                    if let Some(kind) = workflow_kind(prompt) {
                        send_rpc_ok(
                            &mut ws,
                            id,
                            json!({ "ok": true, "instanceId": instance_id }),
                        )
                        .await?;
                        append_n =
                            append_workflow_scenario(&mut ws, &instance_id, append_n, kind).await?;
                        continue;
                    }
                    // C2: the journal user node for a composer send carries the
                    // exact commandId the HTTP response returned, so the web folds
                    // optimistic bubble and transcript node into one.
                    append_n =
                        append_command_user(&mut ws, &instance_id, append_n, prompt, command_id)
                            .await?;
                    // D-027: echo the attachment metadata the Hub resolved, so the
                    // e2e can prove staging reached the Node without a real agent.
                    // 2026-09-15: also echo the [Image #n] manifest (index +
                    // objectId + mediaType in token order) on one line each.
                    // D-027b: the fake Node also pulls each object like the real
                    // one (Bearer host token), lands it under a sanitised name
                    // with a collision suffix, and records the exact
                    // `[File #n] … saved at …` expansion line the harness receives.
                    let sent: Vec<(Option<i64>, String, String, String, String, i64)> = params
                        .get("attachments")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .map(|item| {
                                    (
                                        item.get("index").and_then(Value::as_i64),
                                        item.get("objectId")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned(),
                                        item.get("kind")
                                            .and_then(Value::as_str)
                                            .unwrap_or("image")
                                            .to_owned(),
                                        item.get("mediaType")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned(),
                                        item.get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned(),
                                        item.get("size").and_then(Value::as_i64).unwrap_or(0),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let types = sent
                        .iter()
                        .map(|(_, _, _, media_type, _, _)| media_type.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    let mut reply = if sent.is_empty() {
                        format!("echo: {prompt}")
                    } else {
                        format!("echo: {prompt} [attachments: {types}]")
                    };
                    for (index, object_id, _kind, media_type, _name, _size) in &sent {
                        reply.push_str(&format!(
                            " [attachment-refs: #{} {} {}]",
                            index.unwrap_or(0),
                            object_id,
                            media_type
                        ));
                    }
                    // D-027b: pull + land + expand, exactly like remuda-node.
                    // One line per file, matching the driver's prompt expansion,
                    // so the web transcript can fold each line like an anchor.
                    let mut file_lines: Vec<String> = Vec::new();
                    for (index, object_id, kind, media_type, name, size) in &sent {
                        if kind != "file" {
                            continue;
                        }
                        let landed = land_attachment(addr, &durable_token, object_id, name)
                            .await
                            .unwrap_or_else(|error| format!("<pull failed: {error}>"));
                        file_lines.push(format!(
                            "[File #{}] {} ({}, {}) saved at {}",
                            index.unwrap_or(0),
                            name,
                            media_type,
                            human_size(*size as u64),
                            landed
                        ));
                    }
                    if !file_lines.is_empty() {
                        reply.push('\n');
                        reply.push_str(&file_lines.join("\n"));
                    }
                    if prompt.starts_with("stream ") {
                        // D-028 §7: reply as an open/append chain so the web e2e
                        // sees text arrive mid-turn, the way `MessageDisplay`
                        // deltas do on a real agent-in-PTY session.
                        append_n =
                            append_stream_chunks(&mut ws, &instance_id, append_n, &reply).await?;
                    } else {
                        append_n =
                            append_journal(&mut ws, &instance_id, append_n, "assistant", &reply)
                                .await?;
                    }
                    // D-028 §6: a steer/queue send happens mid-turn; report the
                    // native agent status so the web composer projects working.
                    if params.get("mode").and_then(Value::as_str) == Some("new-turn") {
                        append_n =
                            append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                    } else {
                        append_n = append_native_status(&mut ws, &instance_id, append_n, "working")
                            .await?;
                    }
                    send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                }
                "instance.cancel" => {
                    // §5.3: the turn ends but the instance keeps running.
                    append_n =
                        append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                    send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                }
                "instance.configure" => {
                    // §9.1 model-sync: a model-bearing configure emulates the
                    // `/model` command verdict. Sentinels (posted directly by the
                    // spec, never sent by the UI):
                    //   "__queued__:<id>"  → model-queued lifecycle only
                    //   "__notfound__:<id>"→ model-degraded …:not-found only
                    //   "__resolve__:<alias>=<resolved>" → accepted, alias resolves
                    //   to a different concrete id (the mismatch path)
                    // Otherwise the requested id is accepted verbatim.
                    if let Some(requested) = params.get("model").and_then(Value::as_str) {
                        if let Some(mid) = requested.strip_prefix("__queued__:") {
                            append_n = append_configure_status(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("model-queued:{mid}"),
                            )
                            .await?;
                        } else if let Some(mid) = requested.strip_prefix("__notfound__:") {
                            append_n = append_configure_status(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("model-degraded:{mid}:not-found"),
                            )
                            .await?;
                        } else {
                            let (requested_id, resolved) = if let Some(rest) =
                                requested.strip_prefix("__resolve__:")
                                && let Some((alias, resolved)) = rest.split_once('=')
                            {
                                (alias.to_owned(), resolved.to_owned())
                            } else {
                                (requested.to_owned(), requested.to_owned())
                            };
                            append_n = append_event(
                                &mut ws,
                                &instance_id,
                                append_n,
                                "model",
                                json!({
                                    "requested": requested_id,
                                    "effective": {
                                        "id": resolved,
                                        "source": "remuda",
                                        "observedAt": "2026-09-14T12:00:00.000Z"
                                    },
                                    "raw": resolved
                                }),
                            )
                            .await?;
                        }
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        continue;
                    }
                    // §9.1 effort: emulate a native agent that accepted `/effort`
                    // and whose command verdict reports the level back. When the
                    // requested level differs from what the transcript says, the
                    // fake agent reports a *clamped* level — exactly the
                    // 请求 max → 实际 xhigh path the UI must render.
                    //
                    // Test-only sentinels (the UI never sends these; the spec
                    // posts them directly to exercise the driver's lifecycle
                    // paths without a real PTY):
                    //   "__queued__:<word>"   → effort-queued lifecycle only
                    //   "__degrade__:<word>"  → effort-degraded lifecycle only
                    if let Some(effort) = params.get("effort")
                        && let Some(requested) = effort.get("name").and_then(Value::as_str)
                    {
                        if let Some(word) = requested.strip_prefix("__queued__:") {
                            append_n = append_configure_status(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("effort-queued:{word}"),
                            )
                            .await?;
                        } else if let Some(word) = requested.strip_prefix("__degrade__:") {
                            append_n = append_configure_status(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("effort-degraded:{word}:dialog-kept"),
                            )
                            .await?;
                        } else {
                            // Keep the Claude mismatch fixture; Codex must echo
                            // both max and ultra unchanged through the Hub.
                            let clamped = requested == "max"
                                && instance_kinds.get(&instance_id).map(String::as_str)
                                    == Some("claude");
                            let ultra = requested == "ultracode";
                            // ultracode reads back as tier xhigh on Claude; a
                            // clamped max reads back xhigh too.
                            let tier = if clamped || ultra { "xhigh" } else { requested };
                            let observed_at = monotonic_effort_observed_at();
                            let event = json!({
                                "kind": "effort",
                                "completeness": "structured",
                                "payload": {
                                    "requested": {"name": requested,
                                        "ultracode": effort.get("ultracode").and_then(Value::as_bool).unwrap_or(false)},
                                    "effective": {
                                        // Measured 2.1.272: ultracode carries the
                                        // workflow flag; a plain level accept
                                        // positively clears it; an unrelated clamp
                                        // leaves the flag unknown.
                                        "name": tier,
                                        "ultracode": if ultra {
                                            serde_json::Value::Bool(true)
                                        } else if clamped {
                                            serde_json::Value::Null
                                        } else {
                                            serde_json::Value::Bool(false)
                                        },
                                        "source": "remuda",
                                        "observedAt": observed_at
                                    },
                                    "raw": tier
                                }
                            });
                            append_n = append_event(
                                &mut ws,
                                &instance_id,
                                append_n,
                                "effort",
                                event["payload"].clone(),
                            )
                            .await?;
                        }
                    }
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
                    let answer = params.get("answer").cloned().unwrap_or(json!({}));
                    let removed =
                        if let Some(iid) = params.get("interactionId").and_then(Value::as_str) {
                            pending.lock().await.remove(iid)
                        } else {
                            None
                        };
                    // Reply to the answer RPC FIRST (the Hub gives the Node only
                    // a short RPC budget); journal the harness continuation
                    // afterwards, like the real Node applying owner answers.
                    // The spec waits for the journal record rather than a fixed
                    // sleep, so the late append is observed deterministically.
                    send_rpc_ok(
                        &mut ws,
                        id,
                        json!({ "ok": true, "state": "answer-committed" }),
                    )
                    .await?;
                    if let Some(card) = removed
                        && card.get("kind").and_then(Value::as_str) == Some("question")
                        && let Some(answered_instance) =
                            card.get("instanceId").and_then(Value::as_str)
                    {
                        let summary = answer
                            .get("answers")
                            .map(question_answer_summary)
                            .unwrap_or_default();
                        append_n = append_journal(
                            &mut ws,
                            answered_instance,
                            append_n,
                            "assistant",
                            &format!("AskUserQuestion answered via hook: {summary}"),
                        )
                        .await?;
                        append_n =
                            append_native_status(&mut ws, answered_instance, append_n, "idle")
                                .await?;
                    }
                }
                "workspace.scm.status" | "workspace.scm.diff" | "workspace.scm.file" => {
                    let result = g2_scm_answer(method, &params);
                    send_rpc_ok(&mut ws, id, result).await?;
                }
                "subagent.transcript" => {
                    let agent_id = params.get("agentId").and_then(Value::as_str).unwrap_or("");
                    send_rpc_ok(&mut ws, id, drill_subagent_answer(agent_id)).await?;
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
                            // Trustworthy current mode, the way the Node's
                            // byte-stream scanner reports it for a herdr pane.
                            "altScreen": tty.alt_screen,
                            // Current OSC 9;4 state, or null before any sequence.
                            "progress": tty.progress,
                        }),
                    )
                    .await?;
                }
                "tty.write" => {
                    // Raw keyboard bytes from the browser. Every byte is recorded
                    // (the QuickFind e2e reads the Escape back via the marker) and
                    // printable input is echoed like a cooked PTY. A CR submits
                    // the buffered line as a commandId-less journal user node so
                    // the C2 native-typing e2e can prove it renders once with no
                    // command attribution.
                    let bytes = params
                        .get("dataBase64")
                        .and_then(Value::as_str)
                        .and_then(|raw| base64::engine::general_purpose::STANDARD.decode(raw).ok())
                        .unwrap_or_default();
                    let tty = ttys.entry(instance_id.clone()).or_insert_with(TtyFake::new);
                    // Scripted alt-screen/progress transition: the sentinel
                    // produces the raw frame (so xterm paints it) and a tty.mode
                    // notice exactly like the Node's emulator/scanner relay.
                    if let Some(scripted) = tty.note_input(&bytes) {
                        ws.send(Message::Text(
                            json!({
                                "jsonrpc": "2.0",
                                "method": "tty.frame",
                                "params": {
                                    "instanceId": instance_id,
                                    "streamId": tty.stream_id,
                                    "dataBase64": base64::engine::general_purpose::STANDARD
                                        .encode(&scripted.frame),
                                },
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?;
                        let mut params = json!({
                            "instanceId": instance_id,
                            "streamId": tty.stream_id,
                        });
                        if let Some(alt_screen) = scripted.alt_screen {
                            params["altScreen"] = json!(alt_screen);
                        }
                        if let Some(progress) = scripted.progress {
                            params["progress"] = progress;
                        }
                        ws.send(Message::Text(
                            json!({
                                "jsonrpc": "2.0",
                                "method": "tty.mode",
                                "params": params,
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?;
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        continue;
                    }
                    let submitted = {
                        // note_input() above already recorded the bytes; here we
                        // only echo printable ones and buffer the line for CR.
                        let mut reply = Vec::new();
                        for byte in &bytes {
                            // ESC and CR are control bytes, not display text.
                            if *byte != 0x1b && *byte != b'\r' {
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
                        tty.submit(&bytes)
                    };
                    if let Some(line) = submitted {
                        append_n =
                            append_native_user(&mut ws, &instance_id, append_n, &line).await?;
                        append_n = append_journal(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "assistant",
                            &format!("typed echo: {line}"),
                        )
                        .await?;
                        append_n =
                            append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
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
    }
    Ok(())
}

/// Read frames until the Hub acknowledges the journal append with id `want`.
///
/// Any other frame read while waiting — a stale fire-and-forget append ack or
/// an unrelated Hub RPC like the web poll's interaction.list — is stashed in
/// `queue` for the main loop to process, so waiting on durability can never
/// swallow an RPC.
async fn wait_frame_ack(
    ws: &mut NodeWs,
    queue: &mut std::collections::VecDeque<String>,
    want: &str,
) -> Result<()> {
    loop {
        let frame = match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(_) => anyhow::bail!("hub connection closed waiting for append ack {want}"),
            Err(_) => anyhow::bail!("timed out waiting for append ack {want}"),
        };
        let value: Value = serde_json::from_str(&frame)?;
        let matched =
            value.get("method").is_none() && value.get("id").and_then(Value::as_str) == Some(want);
        if matched {
            return Ok(());
        }
        queue.push_back(frame.to_string());
    }
}

/// Journal the entity lifecycle a terminal-answered question settles with:
/// state `resolved`, actor human with no device, answer carried verbatim.
async fn append_terminal_resolution(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    card: &Value,
    answers: &Value,
) -> Result<u64> {
    let seq = n + 1;
    let iid = card.get("id").and_then(Value::as_str).unwrap_or("int_e2e");
    let mut resolved = card.clone();
    resolved["state"] = json!("resolved");
    resolved["blocking"] = json!(false);
    resolved["answerable"] = json!(false);
    resolved["revision"] = json!("2");
    resolved["answer"] = json!({
        "state": "known",
        "value": {
            "commandId": "cmd_e2e_terminal",
            "actor": {
                "principalId": "prn_e2e_terminal",
                "type": "human",
                "deviceId": null,
                "instanceId": instance_id
            },
            "value": { "kind": "question", "answers": answers },
            "committedAt": "2026-09-16T00:00:02.000Z"
        }
    });
    resolved["resolution"] = json!({
        "state": "known",
        "value": { "reason": "answered", "eventIds": [] }
    });
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": format!("j{seq}"), "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "event": {
                    "kind": "lifecycle",
                    "payload": {
                        "type": "entity",
                        "entityType": "interaction",
                        "entityId": iid,
                        "revision": "2",
                        "previousState": "pending",
                        "state": "resolved",
                        "reasonCode": "terminal-answered",
                        "entity": resolved
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

/// One-line per-question label summary of a card answer, for the assistant
/// journal turn the harness emits after receiving the answers.
fn question_answer_summary(answers: &Value) -> String {
    answers
        .as_object()
        .map(|fields| {
            fields
                .values()
                .map(|field| {
                    let options = field
                        .get("optionIds")
                        .and_then(Value::as_array)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    field
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .unwrap_or(&options)
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default()
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
/// It models a stable per-instance stream id the Hub binds the follower to, a
/// one-screen buffer, and a raw byte log. Receiving an ESC byte is
/// acknowledged with an on-screen marker (`QUICKFIND_ESC_RECEIVED`, QuickFind
/// test); CR flushes the buffered cooked line so the C2 native-typing test can
/// journal it as a commandId-less user node.
struct TtyFake {
    stream_id: String,
    screen: Vec<u8>,
    received: Vec<u8>,
    /// Buffered cooked line; CR flushes it (C2 native-typing journal node).
    line: Vec<u8>,
    /// Current DEC alt-screen mode reported at attach and on `tty.mode`.
    alt_screen: bool,
    /// Current parsed OSC 9;4 progress reported at attach and on edges.
    progress: Option<Value>,
}

/// A scripted renderer notice: the raw frame xterm paints, plus whichever
/// `tty.mode` fields changed (alt screen and/or OSC 9;4 progress).
struct ScriptedFrame {
    frame: Vec<u8>,
    alt_screen: Option<bool>,
    progress: Option<Value>,
}

/// Typing this line (followed by Enter) scripts the fake harness into the
/// DEC alternate screen; the `_OFF` counterpart leaves it. They are plain
/// ASCII sentinels a Playwright keyboard can type into xterm.
const TTY_ALT_ON_SENTINEL: &[u8] = b"TTYMODE_ALT_ON";
const TTY_ALT_OFF_SENTINEL: &[u8] = b"TTYMODE_ALT_OFF";

/// OSC 9;4 progress sentinels (native-config, 2026-09-16). Each emits the raw
/// ConEmu sequence plus a `tty.mode` notice carrying the parsed progress,
/// exactly like the Node's local emulator pump.
const TTY_PROGRESS_INDET_SENTINEL: &[u8] = b"TTYPROG_INDET";
const TTY_PROGRESS_PERCENT_SENTINEL: &[u8] = b"TTYPROG_PERCENT";
const TTY_PROGRESS_ERROR_SENTINEL: &[u8] = b"TTYPROG_ERROR";
const TTY_PROGRESS_DONE_SENTINEL: &[u8] = b"TTYPROG_DONE";

impl TtyFake {
    fn new() -> Self {
        Self {
            stream_id: format!("tty_{}", uuid::Uuid::now_v7()),
            screen: b"fake-harness terminal\r\n$ ".to_vec(),
            received: Vec::new(),
            line: Vec::new(),
            alt_screen: false,
            progress: None,
        }
    }

    /// Scripted sentinels ride the raw input channel. Returns the frame bytes
    /// to push when a sentinel completed this write, plus the changed notices.
    fn note_input(&mut self, bytes: &[u8]) -> Option<ScriptedFrame> {
        self.received.extend_from_slice(bytes);
        let mut tail: Vec<u8> = self
            .received
            .iter()
            .rev()
            .take(32)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        while matches!(tail.last(), Some(b'\r' | b'\n')) {
            tail.pop();
        }
        if tail.ends_with(TTY_ALT_ON_SENTINEL) && !self.alt_screen {
            self.alt_screen = true;
            let frame = b"\x1b[?1049h\x1b[2J\x1b[HFULLSCREEN_FAKE_HARNESS\r\n".to_vec();
            self.screen.extend_from_slice(&frame);
            return Some(ScriptedFrame {
                frame,
                alt_screen: Some(true),
                progress: None,
            });
        }
        if tail.ends_with(TTY_ALT_OFF_SENTINEL) && self.alt_screen {
            self.alt_screen = false;
            let frame = b"\x1b[?1049lINLINE_FAKE_HARNESS\r\n$ ".to_vec();
            self.screen.extend_from_slice(&frame);
            return Some(ScriptedFrame {
                frame,
                alt_screen: Some(false),
                progress: None,
            });
        }
        for (sentinel, sequence, notice) in [
            (
                TTY_PROGRESS_INDET_SENTINEL,
                b"\x1b]9;4;3;\x07".as_slice(),
                json!({"state": "indeterminate"}),
            ),
            (
                TTY_PROGRESS_PERCENT_SENTINEL,
                b"\x1b]9;4;1;50\x07".as_slice(),
                json!({"state": "percent", "percent": 50}),
            ),
            (
                TTY_PROGRESS_ERROR_SENTINEL,
                b"\x1b]9;4;2;\x07".as_slice(),
                json!({"state": "error"}),
            ),
            (
                TTY_PROGRESS_DONE_SENTINEL,
                b"\x1b]9;4;0;\x07".as_slice(),
                json!({"state": "done"}),
            ),
        ] {
            if tail.ends_with(sentinel) && self.progress.as_ref() != Some(&notice) {
                self.progress = Some(notice.clone());
                return Some(ScriptedFrame {
                    frame: sequence.to_vec(),
                    alt_screen: None,
                    progress: Some(notice),
                });
            }
        }
        None
    }

    /// Feed raw bytes; return the trimmed submitted line once CR flushes it.
    fn submit(&mut self, bytes: &[u8]) -> Option<String> {
        self.line.extend_from_slice(bytes);
        if !bytes.contains(&b'\r') {
            return None;
        }
        let line = String::from_utf8_lossy(&self.line)
            .replace('\r', "")
            .trim()
            .to_string();
        self.line.clear();
        (!line.is_empty()).then_some(line)
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

type NodeWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send_rpc_error(ws: &mut NodeWs, id: Value, message: &str) -> Result<()> {
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
    ws: &mut NodeWs,
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
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    Ok(seq)
}

/// One full-shape user message observation. A composer/command send carries
/// `command_id` (C2 correlation); a natively typed prompt omits it.
async fn append_user_message(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
    node: &str,
) -> Result<u64> {
    let seq = n + 1;
    let mut payload = json!({
        "nodeId": node,
        "messageId": node,
        "revision": "1",
        "operation": "open",
        "role": "user",
        "phase": "input",
        "blocks": [{ "type": "text", "text": text }],
        "targetBlock": null,
        "parentToolCallId": null,
        "nativeOrigin": { "state": "known", "value": "ui" },
        "origin": "human",
        "status": "complete",
    });
    if let Some(command_id) = command_id {
        payload["commandId"] = json!(command_id);
    }
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
                    "payload": payload,
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    Ok(seq)
}

/// A command-delivered prompt: its user node carries the delivering commandId.
async fn append_command_user(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    text: &str,
    command_id: Option<&str>,
) -> Result<u64> {
    let node = command_id
        .map(|id| {
            id.strip_prefix("cmd_")
                .map_or_else(|| format!("obj_node_{n}"), |uuid| format!("obj_{uuid}"))
        })
        .unwrap_or_else(|| format!("obj_legacy_{n}"));
    append_user_message(ws, instance_id, n, text, command_id, &node).await
}

/// A prompt typed natively into the PTY: human origin, no commandId, its own
/// node (eventId-derived identity through the hub shorthand normaliser).
async fn append_native_user(ws: &mut NodeWs, instance_id: &str, n: u64, text: &str) -> Result<u64> {
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
                    "payload": { "role": "user", "text": text, "origin": "human" }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    Ok(seq)
}

/// Directory the fake Node lands pulled attachments in (D-027b). Lives for
/// the process so a second send of the same filename observes the collision
/// suffix, exactly like `remuda-node::attachments`.
fn landing_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("remuda-e2e-landed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("landing dir");
        dir
    })
}

/// Defensive sanitisation mirroring `sanitize_attachment_name`: no path
/// separators or control characters.
fn safe_name(raw: &str) -> String {
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || c == '.');
    if trimmed.is_empty() || trimmed.contains(['/', '\\']) || trimmed.chars().any(char::is_control)
    {
        "attachment.bin".to_owned()
    } else {
        trimmed.chars().take(255).collect()
    }
}

/// Pull one object with the host token and land it under a collision-free
/// sanitised name. Returns the absolute landed path.
async fn land_attachment(
    addr: SocketAddr,
    token: &str,
    object_id: &str,
    name: &str,
) -> Result<String> {
    let bytes = reqwest::Client::new()
        .get(format!("http://{addr}/v1/objects/{object_id}"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let dir = landing_dir();
    let wanted = safe_name(name);
    let (stem, ext) = match wanted.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), Some(ext.to_owned())),
        _ => (wanted.clone(), None),
    };
    let mut candidate = wanted.clone();
    let mut suffix = 1u32;
    while dir.join(&candidate).exists() {
        candidate = match &ext {
            Some(ext) => format!("{stem}-{suffix}.{ext}"),
            None => format!("{stem}-{suffix}"),
        };
        suffix += 1;
    }
    let path = dir.join(&candidate);
    std::fs::write(&path, bytes)?;
    Ok(path.display().to_string())
}

/// Compact human size, matching the web chip and the driver expansion.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

async fn append_journal(
    ws: &mut NodeWs,
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

/// A hook-carried approval, shaped like the one the Node builds from a real
/// `PermissionRequest` (D-028 §4.4; payload measured in
/// `docs/design/evidence/native-pty-5.md`).
///
/// Three things differ from the control-channel card above and each is the
/// point of a tier A approval: the carrier is `harness-hook`, the description
/// is the *real* `tool_input` rather than a screen scrape, and there is an
/// always-allow option — which exists only because the harness sent a
/// `permission_suggestion`, so a request without one must not grow one.
fn fake_hook_approval(instance_id: &str, host_id: &str, interaction_id: &str) -> Value {
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
            // The parked hook's identity: a PermissionRequest carries no
            // tool_use_id of its own, so the id is the way back to it.
            "native": { "type": "hook", "invocationId": interaction_id },
            "processGeneration": "1",
            "runGeneration": "1",
            "connectionEpoch": host_id
        },
        "requestVersion": "1",
        "state": "pending",
        "blocking": true,
        "answerable": true,
        "carrier": "harness-hook",
        "request": {
            "kind": "approval",
            "title": "Write",
            "description": "/tmp/hook-approval.txt",
            "toolCallId": null,
            "actionRef": interaction_id,
            "options": [
                { "id": "allow-once", "label": "允许一次", "effect": "allow-once", "nativeValueRef": interaction_id },
                { "id": "allow-always-0", "label": "始终允许 (acceptEdits)", "effect": "allow-session", "nativeValueRef": interaction_id },
                { "id": "deny", "label": "拒绝", "effect": "deny", "nativeValueRef": interaction_id }
            ],
            "requestedPermissionsRef": null,
            "inputDigest": "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
        },
        "deadline": { "state": "unknown", "reason": "none", "evidenceEventIds": [] },
        "deadlineSource": "runtime-policy",
        "answer": { "state": "not-applicable" },
        "delivery": "not-sent",
        "resolution": { "state": "not-applicable" }
    })
}

/// A hook-carried AskUserQuestion, shaped like the one the Node builds from a
/// real `PermissionRequest` whose tool_name is `AskUserQuestion` (measured on
/// claude 2.1.272; `docs/design/evidence/ask-user-question-1.md`).
///
/// Two fields — one radio, one checkbox — with label+description options and
/// free text, exactly what the harness TUI renders. The option id is the
/// label itself: the hook reply's `updatedInput.answers` is keyed by question
/// text and carries labels verbatim.
fn fake_hook_question(instance_id: &str, host_id: &str, interaction_id: &str) -> Value {
    json!({
        "id": interaction_id,
        "revision": "1",
        "createdAt": "2026-09-16T00:00:00.000Z",
        "updatedAt": "2026-09-16T00:00:00.000Z",
        "instanceId": instance_id,
        "runId": null,
        "hostId": host_id,
        "kind": "question",
        "requestKey": {
            "native": { "type": "hook", "invocationId": interaction_id },
            "processGeneration": "1",
            "runGeneration": "1",
            "connectionEpoch": host_id
        },
        "requestVersion": "1",
        "state": "pending",
        "blocking": true,
        "answerable": true,
        "carrier": "harness-hook",
        "request": {
            "kind": "question",
            "title": "AskUserQuestion",
            "fields": [
                {
                    "id": "q0",
                    "title": "接下来这个会话主要想做什么？",
                    "description": "下一步",
                    "input": "single-select",
                    "required": true,
                    "options": [
                        { "id": "继续排查 remuda 环境", "label": "继续排查 remuda 环境", "description": "沿当前环境线索继续排查" },
                        { "id": "回到 GravityDB 开发", "label": "回到 GravityDB 开发", "description": "切回数据库侧开发" },
                        { "id": "验证 shim 与 lease 行为", "label": "验证 shim 与 lease 行为", "description": "跑一轮行为验证" }
                    ],
                    "allowFreeText": true,
                    "sensitive": false
                },
                {
                    "id": "q1",
                    "title": "要把哪些设置记下来？",
                    "description": "记忆",
                    "input": "multi-select",
                    "required": true,
                    "options": [
                        { "id": "保存端口", "label": "保存端口", "description": "记住本次使用的端口" },
                        { "id": "保存环境变量", "label": "保存环境变量", "description": "记住相关环境变量" }
                    ],
                    "allowFreeText": true,
                    "sensitive": false
                }
            ]
        },
        "deadline": { "state": "unknown", "reason": "none", "evidenceEventIds": [] },
        "deadlineSource": "runtime-policy",
        "answer": { "state": "not-applicable" },
        "delivery": "not-sent",
        "resolution": { "state": "not-applicable" }
    })
}

/// r-ux-w: select a synthetic workflow scenario by prompt prefix.
/// r-ux-comment: prompts mentioning code get an assistant reply containing a
/// fenced ts block, so the browser spec can quote it with the 评论 action.
fn code_comment_reply(prompt: &str) -> Option<String> {
    if !prompt.contains("show me code") {
        return None;
    }
    Some(
        "here is the function:\n\
         \n\
         ```ts src/math.ts\n\
         export function add(a: number, b: number): number {\n\
         \x20 return a + b;\n\
         }\n\
         ```\n\
         \n\
         ask about any line."
            .to_owned(),
    )
}

fn workflow_kind(prompt: &str) -> Option<&'static str> {
    const PREFIX: &str = "workflow card";
    if !prompt.starts_with(PREFIX) {
        return None;
    }
    let tail = prompt[PREFIX.len()..].trim();
    Some(match tail {
        "fold" => "fold",
        "fold running" => "fold-running",
        "fail" => "fail",
        "legacy" => "legacy",
        "drill" => "drill",
        "demo running" => "demo-running",
        "demo done" => "demo-done",
        _ => "demo",
    })
}

/// Strictly increasing millisecond RFC3339 timestamp for effort read-back
/// events.
///
/// The web store folds `effort` observations newest-wins by `observedAt` and
/// discards an observation older than the current one. Consecutive switches
/// (ultracode → high → ultracode) can land in the same millisecond, so a real
/// clock could make the second event look older. This stamps each event with
/// at least the current millisecond and a strictly greater value than the
/// previous one, keeping the exact `YYYY-MM-DDTHH:MM:SS.mmmZ` shape the wire
/// Timestamp type requires.
fn monotonic_effort_observed_at() -> String {
    use std::sync::Mutex;
    static LAST_MS: Mutex<i128> = Mutex::new(0);
    let mut last = LAST_MS.lock().expect("effort observed_at lock");
    let now_ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    let ms = now_ms.max(*last + 1);
    *last = ms;
    drop(last);
    // Format by hand: the wire Timestamp type requires exactly
    // `YYYY-MM-DDTHH:MM:SS.mmmZ`, while Rfc3339 strips trailing-zero digits
    // (`.790` renders as `.79`).
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000)
        .expect("millisecond in range");
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond(),
    )
}

/// Append an `instance.configure` native lifecycle with the given status
/// (`effort-queued:<word>` / `effort-degraded:<word>:<reason>` / …).
async fn append_configure_status(
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
                        "nativeName": "instance.configure",
                        "status": status
                    }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    Ok(seq)
}

async fn append_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    kind: &str,
    payload: Value,
) -> Result<u64> {
    append_full_event(
        ws,
        instance_id,
        n,
        json!({ "kind": kind, "payload": payload }),
    )
    .await
}

/// Append a full event object, allowing the scenario to set `source`
/// (c-wfdrill: sub-agent hook observations carry `nativeAgentId`).
async fn append_full_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    n: u64,
    event: Value,
) -> Result<u64> {
    let seq = n + 1;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": format!("j{seq}"),
            "method": "journal.append",
            "params": { "instanceId": instance_id, "event": event }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    Ok(seq)
}

/// A hook-channel observation source stamped with the subagent's native id.
fn hook_agent_source(agent_id: &str) -> Value {
    json!({
        "channel": "hook",
        "delivery": "live",
        "driverKind": "shell-pty",
        "nativeSessionId": { "state": "unknown", "reason": "not-emitted", "evidenceEventIds": [] },
        "nativeAgentId": wf_known(json!(agent_id)),
    })
}

fn wf_known(value: Value) -> Value {
    json!({ "state": "known", "value": value })
}

/// Parse the context-usage-1 sentinel
/// `usage:<input>,<output>,<cacheRead>,<cacheWrite>`; each field is a
/// non-negative integer or `-` for a channel the harness never reported.
/// Returns the full protocol UsagePayload the driver would have appended.
fn scripted_usage(prompt: &str) -> Option<Value> {
    let body = prompt.strip_prefix("usage:")?;
    let parts: Vec<Option<u64>> = body
        .split(',')
        .map(|part| {
            let part = part.trim();
            if part == "-" || part.is_empty() {
                None
            } else {
                part.parse::<u64>().ok()
            }
        })
        .collect();
    if parts.len() != 4 {
        return None;
    }
    let (input, output, cache_read, cache_write) = (parts[0], parts[1], parts[2], parts[3]);
    let knowledge = |value: Option<u64>| match value {
        Some(n) => wf_known(json!(n.to_string())),
        None => wf_unknown(),
    };
    // The driver's total is known only when every component was reported.
    let total = [input, output, cache_read, cache_write]
        .into_iter()
        .collect::<Option<Vec<u64>>>()
        .map(|v| v.iter().sum::<u64>());
    Some(json!({
        "usageId": "obj_e2e_usage",
        "scope": "turn",
        "scopeId": "obj_e2e_run",
        "mode": "snapshot",
        "metricRevision": "1",
        "inputTokens": knowledge(input),
        "inputAccounting": "uncached",
        "outputTokens": knowledge(output),
        "reasoningTokens": wf_unknown(),
        "cacheReadTokens": knowledge(cache_read),
        "cacheWriteTokens": knowledge(cache_write),
        "totalTokens": knowledge(total),
        "cost": { "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] },
        "accounting": "estimated",
        "nativeFieldsRef": null
    }))
}

fn wf_unknown() -> Value {
    json!({ "state": "unknown", "reason": "not-emitted", "evidenceEventIds": [] })
}

/// Build one workflow member observation (r-ux-w timeline card fixture).
#[allow(clippy::too_many_arguments)]
fn wf_member(
    workflow_id: &str,
    member_id: &str,
    phase_id: &str,
    label: &str,
    state: &str,
    revision: u64,
    model: Option<&str>,
    latest_tool: Option<&str>,
    tokens: Option<u64>,
    calls: Option<u64>,
    duration_ms: Option<u64>,
) -> Value {
    json!({
        "workflowId": workflow_id,
        "memberId": member_id,
        "nativeAgentId": wf_known(json!(format!("native-{member_id}"))),
        "nativeKey": wf_unknown(),
        "attempt": wf_known(json!("1")),
        "phaseId": phase_id,
        "label": wf_known(json!(label)),
        "state": state,
        "modelRequested": wf_unknown(),
        "modelResolved": model.map(|m| wf_known(json!(m))).unwrap_or_else(wf_unknown),
        "resultRef": null,
        "revision": revision.to_string(),
        "latestTool": latest_tool.map(|t| wf_known(json!(t))),
        "tokens": tokens.map(|t| json!(t.to_string())),
        "calls": calls.map(|c| json!(c.to_string())),
        "durationMs": duration_ms.map(|d| json!(d.to_string())),
        "startedAt": null,
        "endedAt": null,
    })
}

fn wf_phase(workflow_id: &str, phase_id: &str, label: &str, state: &str) -> Value {
    json!({
        "workflowId": workflow_id,
        "phaseId": phase_id,
        "nativePhaseId": wf_known(json!(phase_id)),
        "label": wf_known(json!(label)),
        "state": state,
        "revision": "1",
        "parentPhaseId": null,
    })
}

#[allow(clippy::too_many_lines)]
async fn append_workflow_scenario(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    mut n: u64,
    kind: &str,
) -> Result<u64> {
    // Stable workflow ids across the running/done two-prompt scenarios.
    let tag = match kind {
        "demo-running" | "demo-done" => "demo",
        "fold-running" => "fold",
        other => other,
    };
    let workflow_id = format!("obj_wf_{tag}");
    let tool_id = format!("obj_wft_{tag}");
    let phase = |i: u8| format!("obj_wfp_{tag}_{i}");
    let member = |i: u8| format!("obj_wfm_{tag}_{i:02}");
    let script = json!({
        "name": format!("card-{tag}"),
        "description": "synthetic timeline card run",
        "phases": [{ "title": "Review" }, { "title": "Verify" }],
    });

    // The Workflow tool call the card hangs on.
    n = append_event(
        ws,
        instance_id,
        n,
        "tool_call",
        json!({
            "nodeId": tool_id,
            "revision": "1",
            "operation": "open",
            "baseRevision": null,
            "toolCallId": tool_id,
            "parentToolCallId": null,
            "toolName": wf_known(json!("Workflow")),
            "displayTitle": wf_known(json!("Workflow")),
            "category": "workflow",
            "input": wf_known(json!({ "script": script.to_string() })),
            "inputTextDelta": null,
            "state": "running",
            "executor": wf_unknown(),
        }),
    )
    .await?;

    let run = |state: &str, revision: u64, totals: Value, live: Value, note: Option<&str>| {
        json!({
            "workflowId": workflow_id,
            "engine": "claude-workflow",
            "nativeRunId": wf_known(json!(format!("wf-native-{tag}"))),
            "nativeTaskId": wf_known(json!(format!("task-{tag}"))),
            "toolCallId": tool_id,
            "state": state,
            "revision": revision.to_string(),
            "title": wf_known(json!(format!("card-{tag}"))),
            "name": wf_known(json!(format!("card-{tag}"))),
            "description": wf_known(json!("synthetic timeline card run")),
            "totals": totals,
            "live": live,
            "note": note,
            "resultRef": null,
        })
    };
    let totals = |done: u64,
                  failed: u64,
                  killed: u64,
                  running: u64,
                  total: u64,
                  total_known: bool,
                  tokens: u64,
                  calls: u64,
                  elapsed_ms: u64| {
        json!({
            "totalKnown": total_known,
            "agentsTotal": total.to_string(),
            "agentsDone": done.to_string(),
            "agentsFailed": failed.to_string(),
            "agentsKilled": killed.to_string(),
            "agentsRunning": running.to_string(),
            "tokens": tokens.to_string(),
            "calls": calls.to_string(),
            "elapsedMs": elapsed_ms.to_string(),
        })
    };
    let live_running = |phase_title: &str, agent: &str| {
        json!({
            "phaseTitle": wf_known(json!(phase_title)),
            "agentLabel": wf_known(json!(agent)),
            "summary": wf_unknown(),
        })
    };
    let live_done = |summary: &str| {
        json!({
            "phaseTitle": wf_unknown(),
            "agentLabel": wf_unknown(),
            "summary": wf_known(json!(summary)),
        })
    };

    if kind == "legacy" {
        // Decision 6: old daemon — run with a note and no phase/member detail.
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.run",
            run(
                "running",
                1,
                json!(null),
                json!(null),
                Some("daemon 版本较旧，暂无阶段明细"),
            ),
        )
        .await?;
        return Ok(n);
    }

    if kind == "drill" {
        return append_drill_scenario(ws, instance_id, n, &workflow_id, &tool_id, &phase, &member)
            .await;
    }

    // Terminal half of the demo scenario (second prompt after "demo running"):
    // only completion revisions, same workflow/tool ids.
    if kind == "demo-done" {
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(2),
                &phase(1),
                "review:security",
                "completed",
                2,
                Some("opus-5[1m]"),
                Some("Grep"),
                Some(44_000),
                Some(8),
                Some(122_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(3),
                &phase(2),
                "verify:auth.ts",
                "completed",
                2,
                Some("haiku-4.5"),
                Some("Bash"),
                Some(31_000),
                Some(5),
                Some(80_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(4),
                &phase(2),
                "verify:api.ts",
                "completed",
                2,
                Some("haiku-4.5"),
                Some("Read"),
                Some(27_000),
                Some(4),
                Some(65_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(1), "Review", "completed"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(2), "Verify", "completed"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.run",
            run(
                "completed",
                2,
                totals(4, 0, 0, 0, 4, true, 412_000, 106, 298_000),
                live_done("Dynamic workflow \"card-demo\" completed"),
                None,
            ),
        )
        .await?;
        return Ok(n);
    }

    if kind == "demo-running" || kind == "demo" {
        let running_only = kind == "demo-running";
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.run",
            run(
                "running",
                1,
                totals(1, 0, 0, 1, 4, true, 69_000, 5, 222_000),
                live_running("Review", "review:security"),
                None,
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(1), "Review", "running"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(2), "Verify", "queued"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(1),
                &phase(1),
                "review:perf",
                "completed",
                1,
                Some("haiku-4.5"),
                Some("Grep"),
                Some(39_000),
                Some(4),
                Some(108_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(2),
                &phase(1),
                "review:security",
                "running",
                1,
                Some("opus-5[1m]"),
                Some("Grep"),
                Some(21_000),
                Some(2),
                Some(62_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(3),
                &phase(2),
                "verify:auth.ts",
                "queued",
                1,
                Some("haiku-4.5"),
                None,
                None,
                None,
                None,
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(4),
                &phase(2),
                "verify:api.ts",
                "queued",
                1,
                Some("haiku-4.5"),
                None,
                None,
                None,
                None,
            ),
        )
        .await?;
        if running_only {
            return Ok(n);
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(2),
                &phase(1),
                "review:security",
                "completed",
                2,
                Some("opus-5[1m]"),
                Some("Grep"),
                Some(44_000),
                Some(8),
                Some(122_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(3),
                &phase(2),
                "verify:auth.ts",
                "completed",
                2,
                Some("haiku-4.5"),
                Some("Bash"),
                Some(31_000),
                Some(5),
                Some(80_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(4),
                &phase(2),
                "verify:api.ts",
                "completed",
                2,
                Some("haiku-4.5"),
                Some("Read"),
                Some(27_000),
                Some(4),
                Some(65_000),
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(1), "Review", "completed"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(2), "Verify", "completed"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.run",
            run(
                "completed",
                2,
                totals(4, 0, 0, 0, 4, true, 412_000, 106, 298_000),
                live_done("Dynamic workflow \"card-demo\" completed"),
                None,
            ),
        )
        .await?;
        return Ok(n);
    }

    if kind == "fold" {
        // One 20-agent phase: the >12-row quiet tail must fold behind 还有 8 个.
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.run",
            run(
                "running",
                1,
                totals(0, 0, 0, 1, 20, true, 0, 0, 1_000),
                live_running("Gen", "gen:batch-01"),
                None,
            ),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(1), "Gen", "running"),
        )
        .await?;
        for i in 0..20 {
            let state = if i == 0 { "running" } else { "queued" };
            n = append_event(
                ws,
                instance_id,
                n,
                "workflow.member",
                wf_member(
                    &workflow_id,
                    &member(i),
                    &phase(1),
                    &format!("gen:batch-{:02}", i + 1),
                    state,
                    1,
                    Some("opus-5[1m]"),
                    if i == 0 { Some("Write") } else { None },
                    if i == 0 { Some(7_200) } else { None },
                    if i == 0 { Some(1) } else { None },
                    if i == 0 { Some(31_000) } else { None },
                ),
            )
            .await?;
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        for i in 0..20 {
            n = append_event(
                ws,
                instance_id,
                n,
                "workflow.member",
                wf_member(
                    &workflow_id,
                    &member(i),
                    &phase(1),
                    &format!("gen:batch-{:02}", i + 1),
                    "completed",
                    2,
                    Some("opus-5[1m]"),
                    Some("Write"),
                    Some(58_000 + u64::from(i) * 200),
                    Some(3),
                    Some(75_000),
                ),
            )
            .await?;
        }
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.phase",
            wf_phase(&workflow_id, &phase(1), "Gen", "completed"),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.run",
            run(
                "completed",
                2,
                totals(20, 0, 0, 0, 20, true, 1_180_000, 60, 391_000),
                live_done("Dynamic workflow \"card-fold\" completed"),
                None,
            ),
        )
        .await?;
        return Ok(n);
    }

    // kind == "fail": 14 done, 1 failed. Failed rows never fold.
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.run",
        run(
            "failed",
            2,
            totals(14, 1, 0, 0, 15, true, 388_000, 94, 361_000),
            live_done("Dynamic workflow \"card-fail\" failed"),
            None,
        ),
    )
    .await?;
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.phase",
        wf_phase(&workflow_id, &phase(1), "Review", "failed"),
    )
    .await?;
    for i in 0..14 {
        n = append_event(
            ws,
            instance_id,
            n,
            "workflow.member",
            wf_member(
                &workflow_id,
                &member(i),
                &phase(1),
                &format!("review:item-{i:02}"),
                "completed",
                1,
                Some("opus-5[1m]"),
                Some("Read"),
                Some(36_000),
                Some(3),
                Some(99_000),
            ),
        )
        .await?;
    }
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.member",
        wf_member(
            &workflow_id,
            &member(99),
            &phase(1),
            "review:security",
            "failed",
            2,
            Some("opus-5[1m]"),
            Some("Bash"),
            Some(22_000),
            Some(2),
            Some(72_000),
        ),
    )
    .await?;
    Ok(n)
}

/// c-wfdrill drill scenario ids — a running member with its own hook tool
/// calls and a queued member whose transcript has not landed yet.
const DRILL_AGENT_SEC: &str = "aae139d44933cefe2";
const DRILL_AGENT_QUEUED: &str = "a600b756a51671bdd";

/// Append the two-phase drill scenario: workflow card plus live sub-agent tool
/// observations that must group UNDER the member rows, keyed by the same
/// native agent ids the `subagent.transcript` RPC answers for.
#[allow(clippy::too_many_arguments)]
async fn append_drill_scenario(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    mut n: u64,
    workflow_id: &str,
    tool_id: &str,
    phase: &impl Fn(u8) -> String,
    member: &impl Fn(u8) -> String,
) -> Result<u64> {
    let mut totals_map = serde_json::Map::new();
    for (k, v) in [
        ("totalKnown", json!(true)),
        ("agentsTotal", json!("2")),
        ("agentsDone", json!(0)),
        ("agentsFailed", json!(0)),
        ("agentsKilled", json!(0)),
        ("agentsRunning", json!(1)),
        ("tokens", json!("83319")),
        ("calls", json!("3")),
        ("elapsedMs", json!("21572")),
    ] {
        totals_map.insert(k.into(), v);
    }
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.run",
        json!({
            "workflowId": workflow_id,
            "engine": "claude-workflow",
            "nativeRunId": wf_known(json!("wf-native-drill")),
            "nativeTaskId": wf_known(json!("task-drill")),
            "toolCallId": tool_id,
            "state": "running",
            "revision": "1",
            "title": wf_known(json!("card-drill")),
            "name": wf_known(json!("card-drill")),
            "description": wf_known(json!("subagent drill-in spike")),
            "totals": json!(totals_map),
            "live": {
                "phaseTitle": wf_known(json!("Review")),
                "agentLabel": wf_known(json!("review:security")),
                "summary": wf_unknown(),
            },
            "note": null,
            "resultRef": null,
        }),
    )
    .await?;
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.phase",
        wf_phase(workflow_id, &phase(1), "Review", "running"),
    )
    .await?;
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.phase",
        wf_phase(workflow_id, &phase(2), "Verify", "queued"),
    )
    .await?;
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.member",
        drill_member(
            workflow_id,
            &member(2),
            &phase(1),
            "review:security",
            "running",
            DRILL_AGENT_SEC,
            "Grep",
            83_319,
            3,
        ),
    )
    .await?;
    n = append_event(
        ws,
        instance_id,
        n,
        "workflow.member",
        drill_member(
            workflow_id,
            &member(3),
            &phase(2),
            "verify:auth.ts",
            "queued",
            DRILL_AGENT_QUEUED,
            "",
            0,
            0,
        ),
    )
    .await?;

    // The running member's own Bash tool activity arrives interleaved on the
    // HOOK channel with agent_id stamped. The web must fold these under the
    // member row instead of flattening them into the main transcript.
    let sub_tool = "toolu_sec_bash1";
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "tool_call",
            "source": hook_agent_source(DRILL_AGENT_SEC),
            "payload": {
                "nodeId": sub_tool,
                "revision": "1",
                "operation": "open",
                "baseRevision": null,
                "toolCallId": sub_tool,
                "parentToolCallId": null,
                "toolName": wf_known(json!("Bash")),
                "displayTitle": wf_known(json!("Bash")),
                "category": "shell",
                "input": wf_known(json!({ "command": "echo 'reviewing auth path'" })),
                "inputTextDelta": null,
                "state": "running",
                "executor": {
                    "state": "known",
                    "value": { "hostId": "hst", "workspaceId": null, "nativeAgentId": DRILL_AGENT_SEC },
                },
            },
        }),
    )
    .await?;
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "tool_result",
            "source": hook_agent_source(DRILL_AGENT_SEC),
            "payload": {
                "nodeId": sub_tool,
                "revision": "2",
                "operation": "close",
                "baseRevision": "1",
                "toolCallId": sub_tool,
                "stage": "final",
                "outcome": "succeeded",
                "blocks": [{ "type": "text", "text": "reviewing auth path\n" }],
                "structuredResult": wf_known(json!({ "stdout": "reviewing auth path\n" })),
                "exitCode": wf_known(json!(0)),
                "changes": [],
            },
        }),
    )
    .await?;

    // A main-agent tool call stays at top level (no agent id on the source).
    n = append_event(
        ws,
        instance_id,
        n,
        "tool_call",
        json!({
            "nodeId": "toolu_main_note",
            "revision": "1",
            "operation": "open",
            "baseRevision": null,
            "toolCallId": "toolu_main_note",
            "parentToolCallId": null,
            "toolName": wf_known(json!("TodoWrite")),
            "displayTitle": wf_known(json!("TodoWrite")),
            "category": "other",
            "input": wf_known(json!({ "todos": [] })),
            "inputTextDelta": null,
            "state": "running",
            "executor": wf_unknown(),
        }),
    )
    .await?;
    Ok(n)
}

/// Drill-scenario member with a deterministic native agent id the drill RPC
/// recognises.
#[allow(clippy::too_many_arguments)]
fn drill_member(
    workflow_id: &str,
    member_id: &str,
    phase_id: &str,
    label: &str,
    state: &str,
    agent_id: &str,
    latest_tool: &str,
    tokens: u64,
    calls: u64,
) -> Value {
    json!({
        "workflowId": workflow_id,
        "memberId": member_id,
        "nativeAgentId": wf_known(json!(agent_id)),
        "nativeKey": wf_unknown(),
        "attempt": wf_known(json!("1")),
        "phaseId": phase_id,
        "label": wf_known(json!(label)),
        "state": state,
        "modelRequested": wf_unknown(),
        "modelResolved": wf_known(json!("claude-opus-5")),
        "resultRef": null,
        "revision": "1",
        "latestTool": if latest_tool.is_empty() { Value::Null } else { wf_known(json!(latest_tool)) },
        "tokens": if tokens > 0 { Some(tokens.to_string()) } else { None },
        "calls": if calls > 0 { Some(calls.to_string()) } else { None },
        "durationMs": Value::Null,
        "startedAt": null,
        "endedAt": null,
    })
}

/// Synthetic answer to the on-demand `subagent.transcript` RPC in the drill
/// scenario. The running member has a sidechain transcript; the queued member
/// answers `available:false` so the UI shows 启动中.
fn drill_subagent_answer(agent_id: &str) -> Value {
    if agent_id != DRILL_AGENT_SEC {
        return json!({ "available": false, "events": [] });
    }
    let transcript_source = json!({
        "channel": "transcript",
        "delivery": "replay",
        "driverKind": "claude-print",
        "nativeAgentId": wf_known(json!(agent_id)),
    });
    let event = |seq: u64, kind: &str, payload: Value| {
        json!({
            "seq": seq.to_string(),
            "kind": kind,
            "source": transcript_source,
            "completeness": "structured",
            "payload": payload,
        })
    };
    let message = |id: &str, role: &str, blocks: Value| {
        json!({
            "nodeId": id,
            "messageId": id,
            "revision": "1",
            "operation": "open",
            "baseRevision": null,
            "role": role,
            "phase": if role == "user" { "input" } else { "final" },
            "blocks": blocks,
            "targetBlock": null,
            "parentToolCallId": null,
            "nativeOrigin": { "state": "known", "value": role },
            "origin": if role == "user" { "human" } else { "assistant" },
            "status": "complete",
        })
    };
    let events = json!([
        event(
            1,
            "message",
            message(
                "m_prompt",
                "user",
                json!([
                    { "type": "text", "text": "Review the auth module for token handling bugs" }
                ])
            )
        ),
        event(2, "tool_call", {
            json!({
                "nodeId": "toolu_agent_grep",
                "revision": "1",
                "operation": "open",
                "baseRevision": null,
                "toolCallId": "toolu_agent_grep",
                "parentToolCallId": null,
                "toolName": wf_known(json!("Grep")),
                "displayTitle": wf_known(json!("Grep")),
                "category": "search",
                "input": wf_known(json!({ "pattern": "token" })),
                "inputTextDelta": null,
                "state": "complete",
                "executor": wf_unknown(),
            })
        }),
        event(3, "tool_result", {
            json!({
                "nodeId": "toolu_agent_grep",
                "revision": "1",
                "operation": "close",
                "baseRevision": null,
                "toolCallId": "toolu_agent_grep",
                "stage": "final",
                "outcome": "succeeded",
                "blocks": [{ "type": "text", "text": "2 matches" }],
                "structuredResult": wf_known(json!({})),
                "exitCode": wf_known(json!(0)),
                "changes": [],
            })
        }),
        event(
            4,
            "message",
            message(
                "m_final",
                "assistant",
                json!([
                    { "type": "text", "text": "auth review done: token refresh race found in sessions.rs" }
                ])
            )
        ),
    ]);
    json!({
        "available": true,
        "meta": {
            "agentId": DRILL_AGENT_SEC,
            "runId": "wf-native-drill",
            "prompt": "Review the auth module for token handling bugs",
            "model": "claude-opus-5",
            "tokens": 83_319,
            "calls": 3,
            "latestTool": "Grep",
            "startedAt": "2026-09-14T10:00:00.000Z",
            "endedAt": "2026-09-14T10:00:21.572Z",
            "finalText": "auth review done: token refresh race found in sessions.rs",
        },
        "events": events,
    })
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
