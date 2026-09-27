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
use std::path::PathBuf;
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
    // c-mobilenew e2e gate: while this file exists the fake Node parks replies
    // to the methods specs use to saturate the Hub's per-link RPC budgets —
    // tty.screen (the bulk-read half) and worktree.list (the control half) —
    // for EVERY instance. Removing the file releases every parked call. It
    // never exists by default, so with no gate every method answers exactly as
    // before. The name is scoped to the listen port: the spec derives the same
    // path from HUB_E2E_LISTEN, so concurrent hubs/specs never share a gate and
    // no temp-dir identity has to be assumed across processes.
    let rpc_gate = std::env::temp_dir().join(format!("remuda-e2e-rpc-gate-{}", addr.port()));
    let _ = std::fs::remove_file(&rpc_gate);
    let (ready_tx, ready_rx) = oneshot::channel();
    let node = tokio::spawn(fake_node(
        addr,
        enroll,
        host_id.clone(),
        pending,
        ready_tx,
        rpc_gate.clone(),
    ));
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
    let mut line = json!({
        "hub": format!("http://{addr}"),
        "token": BOOTSTRAP,
        "hostId": host_id.as_id().as_str(),
        "upstream": format!("http://{upstream_addr}"),
        "viaHost": Value::Null,
    });
    // D-047: an optional second relay-capable Node for the api-route spec.
    let mut route_down_gate: Option<PathBuf> = None;
    if let Some(hub) = &hub
        && let Some((gate, _)) = maybe_spawn_via_proxy(addr, hub, &mut line).await?
    {
        route_down_gate = Some(gate);
    }
    // t-project-switcher: an optional second enrolled fake Node on another
    // host, each host announcing one branded workspace, so the spec can build
    // a project whose members span two hosts (D-024 key never merges).
    let mut project_host_b: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(hub) = &hub {
        project_host_b = maybe_spawn_project_host_b(addr, hub).await?;
    }
    println!("HUB_E2E_READY {line}");
    let _ = io::stdout().flush();
    tokio::signal::ctrl_c().await.ok();
    let _ = std::fs::remove_file(&rpc_gate);
    if let Some(gate) = &route_down_gate {
        let _ = std::fs::remove_file(gate);
    }
    node.abort();
    upstream_task.abort();
    if let Some(host_b) = project_host_b {
        host_b.abort();
    }
    drop(hub);
    Ok(())
}

/// D-047 operator-surface harness (api-route.hub.spec): when
/// `HUB_E2E_API_ROUTE=1`, also enroll a second, relay-capable fake Node that
/// acts purely as a proxy host `H`. Its host id is published in the
/// HUB_E2E_READY line as `viaHost`.
///
/// The down gate (a port-scoped temp file, same convention as the rpc gate)
/// is the smallest knob for the §B.5 scenario: while the file exists the
/// proxy socket is closed, so the Hub's link-lost hook marks every via
/// instance `blocked{api-route-down}` and publishes the diagnostic the strip
/// renders as an error. Deleting the file reconnects the proxy with a new
/// epoch; the block is deliberately not cleared by a reconnect, matching the
/// Hub's "operator re-dispatches" rule.
async fn maybe_spawn_via_proxy(
    addr: SocketAddr,
    hub: &remuda_hub::RunningHub,
    ready_line: &mut Value,
) -> Result<Option<(PathBuf, HostId)>> {
    if std::env::var("HUB_E2E_API_ROUTE").as_deref() != Ok("1") {
        return Ok(None);
    }
    let down_gate = std::env::temp_dir().join(format!("remuda-e2e-route-down-{}", addr.port()));
    let _ = std::fs::remove_file(&down_gate);
    let via_host = HostId::new();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .context("mint via-host enroll token")?;
    let gate = down_gate.clone();
    let proxy = via_host.clone();
    tokio::spawn(async move {
        if let Err(error) = via_proxy_node(addr, enroll, proxy.clone(), gate).await {
            eprintln!("via proxy node {proxy:?} exited: {error:#}");
        }
    });
    ready_line["viaHost"] = json!(via_host.as_id().as_str());
    Ok(Some((down_gate, via_host)))
}

/// Connect/reconnect loop for the D-047 proxy fake Node.
async fn via_proxy_node(
    addr: SocketAddr,
    enroll: String,
    host_id: HostId,
    down_gate: PathBuf,
) -> Result<()> {
    let mut epoch = 1u64;
    loop {
        // Down: hold the link closed while the gate file exists. The Hub
        // observes an immediate TCP close on the serving arm below; this arm
        // covers "start while already down" and post-reconnect re-drops.
        while down_gate.exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            epoch = epoch.wrapping_add(1);
        }
        let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
                "params": {
                    "hostId": host_id.as_id().as_str(),
                    "nodeVersion": "0.1.0-e2e",
                    "label": "e2e-via-host",
                    "nodeEpoch": format!("epoch-e2e-via-{epoch}"),
                    "instanceStoreFound": true,
                    "instances": [],
                    "host": {
                        "hostname": "e2e-via-host.local",
                        "workspaceRevision": 1,
                        "workspaces": [],
                        "maxInstances": 8,
                    },
                    // This Node exists to serve the proxy half; the worker
                    // host advertises its own capability via HUB_E2E_API_ROUTE.
                    "capabilities": { "apiRelay": true }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
        // Serve until the gate drops or the socket ends. api.egress installs
        // are notifications (no id) and need no answer; answer any RPC ok.
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if down_gate.exists() {
                        // Drop cleanly so the Hub's link-lost hook runs at
                        // once; the outer loop reconnects when the gate clears.
                        epoch = epoch.wrapping_add(1);
                        break;
                    }
                }
                frame = ws.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(frame) = serde_json::from_str::<Value>(&text)
                                && let Some(id) = frame.get("id").cloned()
                            {
                                let _ = ws.send(Message::Text(
                                    json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}})
                                        .to_string()
                                        .into(),
                                )).await;
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
            }
        }
        let _ = ws.close(None).await;
        epoch = epoch.wrapping_add(1);
        // Avoid a tight loop when the socket fails before the gate exists.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// t-project-switcher harness: when `HUB_E2E_PROJECT_SWITCHER=1`, enroll a
/// second fake Node on its own host (`e2e-project-host-b`) that announces one
/// branded workspace (root `/tmp/remuda-project-b`). Together with the
/// primary fake node's `/tmp/remuda-project-a` workspace the spec creates a
/// project whose `members[]` span two hosts — the D-024 case where the same
/// logical project must NOT collapse into one Space key. The node answers
/// every RPC ok; no session is ever launched on it in the spec.
async fn maybe_spawn_project_host_b(
    addr: SocketAddr,
    hub: &remuda_hub::RunningHub,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    if std::env::var("HUB_E2E_PROJECT_SWITCHER").as_deref() != Ok("1") {
        return Ok(None);
    }
    let host_b = HostId::new();
    let workspace_b = remuda_protocol::WorkspaceId::new();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .context("mint host-b enroll token")?;
    let handle = tokio::spawn(async move {
        if let Err(error) = project_host_b_node(addr, enroll, host_b.clone(), workspace_b).await {
            eprintln!("project host-b node {host_b:?} exited: {error:#}");
        }
    });
    Ok(Some(handle))
}

/// Connect/reconnect loop for the second enrolled fake Node.
async fn project_host_b_node(
    addr: SocketAddr,
    enroll: String,
    host_id: HostId,
    workspace_id: remuda_protocol::WorkspaceId,
) -> Result<()> {
    let mut epoch = 1u64;
    loop {
        let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
                "params": {
                    "hostId": host_id.as_id().as_str(),
                    "nodeVersion": "0.1.0-e2e",
                    "label": "e2e-project-host-b",
                    "nodeEpoch": format!("epoch-e2e-project-b-{epoch}"),
                    "instanceStoreFound": true,
                    "instances": [],
                    "host": {
                        "hostname": "e2e-project-host-b.local",
                        "workspaceRevision": 1,
                        "workspaces": [{
                            "workspaceId": workspace_id.as_id().as_str(),
                            "hostId": host_id.as_id().as_str(),
                            "root": "/tmp/remuda-project-b"
                        }],
                        "labels": { "role": "e2e" },
                        "maxInstances": 8,
                        "herdr": { "version": "e2e-fake" }
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
        // Inventory-only node: the spec only reads its registry (host +
        // workspace) and never launches here, but GET /v1/hosts/{id}/workspaces
        // issues a live workspace.list RPC, so answer it with the same
        // snapshot shape the hello carries; every other RPC gets a generic ok.
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(frame) = serde_json::from_str::<Value>(&text)
                        && let Some(id) = frame.get("id").cloned()
                    {
                        let method = frame
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let result = if method == "workspace.list" {
                            json!({
                                "workspaceRevision": 1,
                                "workspaces": [{
                                    "workspaceId": workspace_id.as_id().as_str(),
                                    "hostId": host_id.as_id().as_str(),
                                    "root": "/tmp/remuda-project-b"
                                }]
                            })
                        } else {
                            json!({"ok": true})
                        };
                        let _ = ws
                            .send(Message::Text(
                                json!({"jsonrpc":"2.0","id":id,"result":result})
                                    .to_string()
                                    .into(),
                            ))
                            .await;
                    }
                }
                Some(Ok(_)) => {}
                _ => break,
            }
        }
        epoch = epoch.wrapping_add(1);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

/// §9.1 model-sync: the gateway catalog the launch snapshot carries. A prompt
/// containing `tall-catalog` gets an 81-id catalog (launch + 80 rows) so the
/// ux-modelpick e2e can prove anchoring, scrolling and read-back with a list
/// taller than the viewport. Returns the JSON payload fragment and the id
/// vector (also remembered per instance for listed/typed attribution).
fn fake_launch_catalog(prompt: Option<&str>) -> (Value, Vec<String>) {
    let models: Vec<String> = if prompt.is_some_and(|text| text.contains("tall-catalog")) {
        std::iter::once("e2e/auto".to_owned())
            .chain((0..80).map(|i| format!("e2e/m-{i:02}")))
            .collect()
    } else {
        vec![
            "e2e/auto".to_owned(),
            "e2e/fast".to_owned(),
            "e2e/plain".to_owned(),
            "claude-e2e-only".to_owned(),
        ]
    };
    let json = json!({
        "models": models,
        "source": "gateway-discovery",
        "observedAt": "2026-09-14T12:00:00.000Z",
        "cache": {
            "scope": "scoped-config-dir",
            "baseUrl": "https://relay.e2e.invalid/v1",
            "fetchedAt": "2026-09-14T12:00:00.000Z"
        },
        "discoveryEnv": true
    });
    (json, models)
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

/// The `node.hello` frame the harness sends, enroll and restart alike.
///
/// `live` is the instance inventory the Node announces. A restart reports what
/// survived it — nothing, for an in-process carrier — which is what the Hub
/// diffs against the epoch it recorded. See `NODE_RESTART_SENTINEL`.
fn node_hello_frame(host_id: &HostId, workspaces: &Value, epoch: u64, live: &[String]) -> Value {
    let host = host_id.as_id().as_str();
    let inventory: Vec<Value> = live
        .iter()
        .map(|instance_id| json!({ "id": instance_id, "hostId": host, "lifecycle": "running" }))
        .collect();
    json!({
        "jsonrpc": "2.0",
        "id": "hello",
        "method": "node.hello",
        "params": {
            "hostId": host,
            "nodeVersion": "0.1.0-e2e",
            "label": "e2e-fake-node",
            // A new epoch on every restart is what makes the Hub reconcile;
            // an unchanged epoch is a reconnect and settles nothing.
            "nodeEpoch": format!("epoch-e2e-{epoch}"),
            "instances": inventory,
            // This fake node always found its store, so an empty inventory is
            // a claim the Hub may act on. A real Node that cannot say this
            // omits the key instead (see `DevNode::announceable_inventory`).
            "instanceStoreFound": true,
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
                ],
                // D-047 operator-surface knob (api-route.hub.spec): when
                // HUB_E2E_API_ROUTE=1 the fake worker also claims the api.*
                // relay class, so a `via` create is accepted and its create
                // result echoes the requested apiRoute. Absent by default —
                // every non-route spec then hits api-via-unsupported exactly
                // as a pre-D-048 Node would.
                "apiRelay": std::env::var("HUB_E2E_API_ROUTE").as_deref() == Ok("1"),
            }
        }
    })
}

/// Methods whose replies park while the e2e gate file exists. They are the
/// fan-out bulk read (tty.screen) and the cheap control RPCs the Hub issues
/// before GET /v1/hosts/{id}/workspaces and /v1/worktrees (workspace.list and
/// worktree.list), so a spec can fill the control half of the per-link budget
/// with calls that park for their full node timeout.
const GATED_METHODS: &[&str] = &["tty.screen", "workspace.list", "worktree.list"];

/// c-ghostbadge: drop the current WebSocket and reconnect with a NEW epoch,
/// re-announcing exactly `survivors` as live. A new epoch is what makes the
/// Hub run reconcile_reported_instances: every created instance missing from
/// `survivors` settles exited (`node-epoch-changed`). The caller owns which
/// ids survive; the durable host token re-authenticates the reconnect (a
/// second node.hello on one socket is rejected, hence the new connection).
///
/// Frames arriving before the hello reply share the socket with unrelated Hub
/// RPCs; they are stashed into `frame_queue`, never swallowed.
async fn node_restart_reconnect(
    addr: SocketAddr,
    durable_token: &str,
    host_id: &HostId,
    workspaces: &Value,
    node_epoch: &mut u64,
    survivors: Vec<String>,
    frame_queue: &mut std::collections::VecDeque<String>,
) -> Result<NodeWs> {
    *node_epoch += 1;
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {durable_token}")
            .parse()
            .context("authorization header")?,
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    ws.send(Message::Text(
        node_hello_frame(host_id, workspaces, *node_epoch, &survivors)
            .to_string()
            .into(),
    ))
    .await?;
    loop {
        let Some(Ok(Message::Text(text))) =
            tokio::time::timeout(Duration::from_secs(5), ws.next()).await?
        else {
            anyhow::bail!("hub closed during the fake node restart");
        };
        let value: Value = serde_json::from_str(&text)?;
        if value.get("id").and_then(Value::as_str) == Some("hello") {
            anyhow::ensure!(
                value.get("result").is_some(),
                "restart hello rejected: {value}"
            );
            return Ok(ws);
        }
        frame_queue.push_back(text.to_string());
    }
}

/// Build the D-047 create-result `apiRoute` echo from the requested spec.
///
/// The fake Node stands in for the real listener half: it accepts a via
/// launch and reports which route the launch took. `auto` reaching this fake
/// has already been resolved by the Hub (it writes `hub-relay` when the proxy
/// host has no relayBind), so `auto` maps to `hub-relay` here as well. The
/// Hub validates this echo against the requested route and projects only an
/// echo it accepts (D-035), which is what the Session strip reads.
fn api_route_echo(spec: &Value) -> Option<Value> {
    let requested = spec.get("apiRoute")?;
    if requested.get("mode").and_then(Value::as_str) != Some("via") {
        return None;
    }
    let kind = match requested.get("route").and_then(Value::as_str) {
        Some("direct-net") => "direct-net",
        _ => "hub-relay",
    };
    let mut echo = json!({ "mode": "via", "route": kind });
    // `self` (Hub-host proxy) carries no host id; a host proxy must echo the
    // exact id the Hub named, or the Hub refuses to project the route.
    if let Some(via_host_id) = requested.get("viaHostId") {
        echo["viaHostId"] = via_host_id.clone();
    }
    Some(echo)
}

/// Build the `instance.create`/`instance.resume` result.
///
/// Default is byte-identical to the pre-D-047 fake (`ok` + `instanceId`
/// only): the Hub then leaves `node_accepted()` false, as it always has, and
/// every existing spec converges through the mirrored journal. Only with
/// HUB_E2E_API_ROUTE=1 does the reply carry `accepted: true` (so the Hub's
/// create projection runs) and the Node-echoed `apiRoute` — both are the
/// D-047 fixture this file adds, and neither must change the default state
/// machine.
fn create_result(spec: &Value, instance_id: &str) -> Value {
    if std::env::var("HUB_E2E_API_ROUTE").as_deref() != Ok("1") {
        return json!({ "ok": true, "instanceId": instance_id });
    }
    let mut result = json!({ "accepted": true, "ok": true, "instanceId": instance_id });
    if let Some(echo) = api_route_echo(spec) {
        result["apiRoute"] = echo;
    }
    result
}

async fn fake_node(
    addr: SocketAddr,
    enroll: String,
    host_id: HostId,
    pending: Arc<Mutex<HashMap<String, Value>>>,
    ready: oneshot::Sender<()>,
    rpc_gate: PathBuf,
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
    let mut workspaces = vec![
        json!({ "workspaceId": "wsp_e2e", "hostId": host, "root": "/tmp/remuda-e2e" }),
        json!({ "workspaceId": "wsp_e2e_second", "hostId": host, "root": "/tmp/remuda-e2e-second" }),
        // G2 files-view synthetic scenarios (docs/design/files-view-contract.md §3.6).
        json!({ "workspaceId": "wsp_g2_changes", "hostId": host, "root": "/tmp/remuda-g2/changes" }),
        json!({ "workspaceId": "wsp_g2_clean", "hostId": host, "root": "/tmp/remuda-g2/clean" }),
        json!({ "workspaceId": "wsp_g2_nogit", "hostId": host, "root": "/tmp/remuda-g2/nogit" }),
        json!({ "workspaceId": "wsp_g2_denied", "hostId": host, "root": "/tmp/remuda-g2/denied" }),
        json!({ "workspaceId": "wsp_g2_trunc", "hostId": host, "root": "/tmp/remuda-g2/trunc" }),
        json!({ "workspaceId": "wsp_g2_changed", "hostId": host, "root": "/tmp/remuda-g2/changed" }),
    ];
    // D-047 operator knob (HUB_E2E_API_ROUTE=1, api-route.hub.spec): project
    // members require a branded workspace id from the host's acknowledged
    // snapshot; the legacy `wsp_e2e` labels are deliberately non-branded.
    // Announce one extra branded workspace on the e2e root (the hello and
    // every workspace.list reply share this list, so the Hub never observes a
    // revision-less change). Scoped to the flag; other specs see the unchanged
    // snapshot.
    if std::env::var("HUB_E2E_API_ROUTE").as_deref() == Ok("1") {
        workspaces.push(json!({
            "workspaceId": remuda_protocol::WorkspaceId::new().as_id().as_str(),
            "hostId": host,
            "root": "/tmp/remuda-e2e"
        }));
    }
    // t-bind: project membership needs a branded workspace id, so announce one
    // extra workspace behind the bind trigger. It uses a distinct root
    // (/tmp/remuda-bind) so the mobile home's per-root space chips never merge
    // with or shadow the legacy wsp_e2e "remuda-e2e" chip other specs click.
    if task_bind_enabled() {
        workspaces.push(json!({
            "workspaceId": remuda_protocol::WorkspaceId::new().as_id().as_str(),
            "hostId": host,
            "root": "/tmp/remuda-bind"
        }));
    }
    // t-project-switcher: the cross-host project needs one branded workspace
    // on this, the primary fake host. Distinct root (/tmp/remuda-project-a) so
    // it never merges with another spec's per-root space chips (D-024 is what
    // the spec asserts; the fixture keeps the two members visibly apart).
    if project_switcher_enabled() {
        workspaces.push(json!({
            "workspaceId": remuda_protocol::WorkspaceId::new().as_id().as_str(),
            "hostId": host,
            "root": "/tmp/remuda-project-a"
        }));
    }
    let workspaces = Value::Array(workspaces);
    // The enroll hello announces an empty inventory — this process holds no
    // sessions yet — and a later restart re-announces whatever is still live.
    // The Hub reads a missing key as "cannot enumerate" and an empty array as
    // "owns nothing", so "no sessions yet" is spelled `[]` deliberately.
    let mut node_epoch = 1u64;
    ws.send(Message::Text(
        node_hello_frame(&host_id, &workspaces, node_epoch, &[])
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
    seed_host_file_fixtures()?;
    let mut append_n = 0u64;
    // Minimal PTY harness for the xterm e2e specs. A terminal session is
    // registered on create, tty.attach returns its stable per-instance stream
    // + screen, and tty.write records raw bytes (QuickFind proves an Escape
    // reached the process) and echoes printable input; CR additionally submits
    // a commandId-less journal user node (C2 native-typing correlation).
    let mut ttys: HashMap<String, TtyFake> = HashMap::new();
    // Instances whose create explicitly opted into cooked screen replies on
    // `tty.screen`. Other instances keep the pre-existing empty-screen
    // answer (their rows fall back to journal text), so adding the screen
    // arm does not change any existing spec.
    let mut screen_enabled: HashSet<String> = HashSet::new();
    // A failed Claude launch must not acquire a TTY through lazy attach.
    let mut claude_ptys = HashSet::new();
    let mut instance_kinds: HashMap<String, String> = HashMap::new();
    // §9.1 model-sync: per-instance catalog ids remembered at launch so a
    // configure verdict can attribute the switch path (listed vs typed).
    let mut model_catalogs: HashMap<String, Vec<String>> = HashMap::new();
    // t-bind gated worktree layer (HUB_E2E_TASK_BIND=1).
    let mut bind_state = BindLeaseState::seeded();
    // Scripted terminal answers: a spawned timer sends (instance, iid, answers)
    // back into this loop so journal appends stay single-writer.
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<(String, String, Value)>(8);
    // Scripted terminal answers awaiting their first `interaction.list`, keyed
    // by interaction id -> instance id. See the `ask-question-terminal` arm.
    let terminal_pending: Arc<Mutex<HashMap<String, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Frames read while waiting for a journal.append ack (they may be stale
    // append responses or — importantly — unrelated Hub RPCs such as the
    // web poll's interaction.list). Never drop RPCs: reprocess them as soon
    // as the current handler returns.
    let mut frame_queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    // Parked frames re-enter this loop verbatim once the gate file is removed;
    // they are then processed by their normal arm, so the reply is exactly the
    // ungated one. The park task never touches the socket itself.
    let (requeue_tx, mut requeue_rx) = tokio::sync::mpsc::channel::<String>(64);
    loop {
        tokio::select! {
            biased;
            Some(frame) = requeue_rx.recv() => {
                frame_queue.push_back(frame);
            }
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
            // Gate switch: while the file exists, park the frame (it already
            // occupies a Hub pending slot) and reprocess it verbatim once the
            // file is removed, so its normal arm — and reply — is unchanged.
            if GATED_METHODS.contains(&method) && rpc_gate.exists() {
                let parked = text.clone();
                let gate = rpc_gate.clone();
                let requeue = requeue_tx.clone();
                tokio::spawn(async move {
                    let step = Duration::from_millis(100);
                    let max = Duration::from_secs(60);
                    let mut waited = Duration::ZERO;
                    while gate.exists() && waited < max {
                        tokio::time::sleep(step).await;
                        waited += step;
                    }
                    let _ = requeue.send(parked).await;
                });
                continue;
            }
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
                // t-bind: in-memory worktree catalog/lease, gated on its own
                // trigger so other specs see no worktree behaviour.
                "worktree.list" if task_bind_enabled() => {
                    send_rpc_ok(&mut ws, id, bind_state.list()).await?;
                }
                "worktree.lease" if task_bind_enabled() => match bind_state.lease(&params) {
                    Ok(result) => send_rpc_ok(&mut ws, id, result).await?,
                    Err(message) => send_rpc_error(&mut ws, id, &message).await?,
                },
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
                        let launch_prompt = spec
                            .pointer("/initialInput/text")
                            .or_else(|| spec.get("prompt"))
                            .and_then(Value::as_str);
                        let (catalog_json, catalog_ids) = fake_launch_catalog(launch_prompt);
                        model_catalogs.insert(instance_id.clone(), catalog_ids);
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
                                "catalog": catalog_json
                            }),
                        )
                        .await?;
                        send_rpc_ok(&mut ws, id, create_result(spec, &instance_id)).await?;
                        continue;
                    }
                    if method == "instance.resume" {
                        // Report the child ready with the inherited native
                        // session id so its Hub row goes live and the phone
                        // home can navigate straight to the new instance
                        // (D-026). The claude-pty branch above handles its own
                        // provider-overlay-gated launch.
                        let session_id = spec
                            .get("resumeSessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                        append_n = append_instance_state(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "ready",
                            Some(&session_id),
                        )
                        .await?;
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
                        // Opt-in: a create whose initial input carries the
                        // `screen-read` sentinel gets real cooked screen lines
                        // from `tty.screen` (c-nextstep key round trip). Every
                        // other terminal keeps the empty-screen default answer.
                        let terminal_prompt = params
                            .pointer("/initialInput/text")
                            .or_else(|| params.get("prompt"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if terminal_prompt.contains("screen-read") {
                            screen_enabled.insert(instance_id.clone());
                        }
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
                    // c-mhome: an exited row carrying an error. The instance
                    // reports a native session id first (D-026 resume needs a
                    // transcript to continue), then exits with `lastError`,
                    // which is the text the phone home puts in the row body.
                    if prompt.contains("mhome-exit") {
                        let session_id = format!("mhome-exit-{}", uuid::Uuid::now_v7());
                        append_n = append_instance_state(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "ready",
                            Some(&session_id),
                        )
                        .await?;
                        append_n = append_instance_exit(
                            &mut ws,
                            &instance_id,
                            append_n,
                            &session_id,
                            "API Error: MHOME_EXIT_SENTINEL (429)",
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
                    // c-ghostbadge round 2: a GENUINELY live hook approval
                    // with a short known deadline. The card is journaled
                    // durably AND held in the live broker, so badge and
                    // inbox both show 1/1 while the agent is blocked. The
                    // spec then ends the instance through a REAL node
                    // restart (instance.send sentinel `GHOSTNODE_RESTART`):
                    // the new epoch omits the instance, the Hub's
                    // reconcile_reported_instances settles it exited, the
                    // new process serves no interaction.list for it, and
                    // once the deadline crosses the shared projection drops
                    // the durable pending row to expired — 0/0 with no
                    // reload. Nothing here is pre-ended.
                    if prompt.contains("ghostbadge-live") {
                        let iid = InteractionId::new();
                        // Long enough that the e2e's create -> restart ->
                        // reconcile path finishes while the card is still
                        // open (it asserts 1/1 before watching the flip);
                        // the spec waits on the flip with a generous timeout.
                        let deadline_ts =
                            time::OffsetDateTime::now_utc() + time::Duration::seconds(45);
                        let deadline = format!(
                            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                            deadline_ts.year(),
                            deadline_ts.month() as u8,
                            deadline_ts.day(),
                            deadline_ts.hour(),
                            deadline_ts.minute(),
                            deadline_ts.second(),
                            deadline_ts.millisecond(),
                        );
                        let mut card = fake_approval(&instance_id, host, iid.as_id().as_str());
                        card["deadline"] = json!({ "state": "known", "value": deadline });
                        card["deadlineSource"] = json!("runtime-policy");
                        append_n = append_interaction_requested(
                            &mut ws,
                            &mut frame_queue,
                            &instance_id,
                            append_n,
                            &card,
                        )
                        .await?;
                        // Live broker agrees with the durable journal while
                        // the process runs (the real Node serves both).
                        pending
                            .lock()
                            .await
                            .insert(iid.as_id().as_str().to_string(), card);
                        append_n = append_native_status(&mut ws, &instance_id, append_n, "blocked")
                            .await?;
                        send_rpc_ok(
                            &mut ws,
                            id,
                            json!({ "ok": true, "instanceId": instance_id }),
                        )
                        .await?;
                        continue;
                    }
                    // c-mfix round 2: the full phone-chrome combo fixture
                    // (exited resumable instance + live hook/tool/status strip).
                    if prompt == "mfix-chrome-combo" {
                        send_rpc_ok(
                            &mut ws,
                            id,
                            json!({ "ok": true, "instanceId": instance_id }),
                        )
                        .await?;
                        append_n =
                            append_mfix_chrome_combo(&mut ws, &instance_id, append_n).await?;
                        continue;
                    }
                    let interaction_id = InteractionId::new();
                    // c-nextstep list-row phrase: a create prompt with the
                    // `workflow card <scenario> row-phrase` form raises NO
                    // approval and journals that workflow scenario directly,
                    // staying in native status working. `row-phrase` is the
                    // explicit sentinel; only a running scenario leaves a
                    // phrase, so unknown scenarios raise the normal approval.
                    let row_scenario: Option<&str> = prompt
                        .strip_prefix("workflow card ")
                        .and_then(|rest| rest.strip_suffix(" row-phrase"))
                        .filter(|scenario| *scenario == "demo-running" || *scenario == "live");
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
                    if row_scenario.is_none() {
                        pending
                            .lock()
                            .await
                            .insert(interaction_id.as_id().as_str().to_string(), card);
                    }
                    if terminal_answer {
                        // Model the human answering in the agent's own TUI: the
                        // harness closes the dialog itself (PostToolUse with
                        // answers), with no interaction.answer RPC.
                        //
                        // Armed, not started: a fixed delay from *create* raced
                        // the spec, which asserts the card is pending after the
                        // form is on screen. The web store polls
                        // `interaction.list` every 2 s, so on a loaded lane the
                        // form can still be rendering from a poll while this
                        // timer has already retired the card — the spec then
                        // reads 0 pending and fails, exactly as the Linux gate
                        // saw. Waiting for the first list that carries this card
                        // means the answer can only land after a client could
                        // see it, on any machine.
                        terminal_pending.lock().await.insert(
                            interaction_id.as_id().as_str().to_string(),
                            instance_id.clone(),
                        );
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
                    } else if row_scenario.is_none() {
                        // c-nextstep row rows carry a workflow scenario below;
                        // its own events must be the latest journal content so
                        // the list projects the run phrase, not this echo.
                        append_n = append_journal(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "assistant",
                            &format!("echo: {prompt}"),
                        )
                        .await?;
                    }
                    // The approval raised below keeps the create blocked on a
                    // human until answered; answering journals idle (turn
                    // complete). c-steer: the "blocked-question" sentinel
                    // instead projects `blocked` directly — a pending question
                    // is never `working`, so the composer must not mistake it
                    // for a turn. c-mhome's "mhome-blocked" row is blocked the
                    // same way, after one usage observation so its home row has
                    // a known 50% context ring (100k of the 200k Claude window).
                    if prompt.contains("mhome-blocked") {
                        if let Some(usage) = scripted_usage("usage:40000,500,60000,0") {
                            append_n =
                                append_event(&mut ws, &instance_id, append_n, "usage", usage)
                                    .await?;
                        }
                        append_n = append_native_status(&mut ws, &instance_id, append_n, "blocked")
                            .await?;
                    } else if prompt.contains("blocked-question") {
                        append_n = append_native_status(&mut ws, &instance_id, append_n, "blocked")
                            .await?;
                    } else {
                        append_n = append_native_status(&mut ws, &instance_id, append_n, "working")
                            .await?;
                    }
                    if let Some(scenario) = row_scenario {
                        // Running workflow events follow the working status, so
                        // the row projects a live-run phrase from the journal.
                        append_n =
                            append_workflow_scenario(&mut ws, &instance_id, append_n, scenario)
                                .await?;
                    }
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
                        let (catalog_json, catalog_ids) = fake_launch_catalog(Some(prompt));
                        model_catalogs.insert(instance_id.clone(), catalog_ids);
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
                                "catalog": catalog_json
                            }),
                        )
                        .await?;
                    }
                    send_rpc_ok(&mut ws, id, create_result(spec, &instance_id)).await?;
                }
                "instance.send" => {
                    let prompt = params
                        .get("prompt")
                        .or_else(|| params.pointer("/text"))
                        .and_then(Value::as_str)
                        .unwrap_or("hello");
                    let command_id = params.get("commandId").and_then(Value::as_str);
                    // c-ghostbadge round 2: end THIS instance for real. Ack
                    // the send, drop the instance's in-memory live cards (a
                    // restarted process has no broker memory), then reconnect
                    // under a new epoch re-announcing every OTHER created
                    // instance as a survivor. The Hub's epoch reconcile then
                    // settles only this instance exited while other specs'
                    // live sessions stay untouched.
                    if prompt == GHOST_RESTART_SENTINEL {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        pending.lock().await.retain(|_iid, card| {
                            card.get("instanceId").and_then(Value::as_str)
                                != Some(instance_id.as_str())
                        });
                        let survivors: Vec<String> = instance_kinds
                            .keys()
                            .filter(|id| id.as_str() != instance_id.as_str())
                            .cloned()
                            .collect();
                        let _ = ws.close(None).await;
                        ws = node_restart_reconnect(
                            addr,
                            &durable_token,
                            &host_id,
                            &workspaces,
                            &mut node_epoch,
                            survivors,
                            &mut frame_queue,
                        )
                        .await?;
                        continue;
                    }
                    // c-journalpage bounded-window seeding hook:
                    // `__journal_burst__:<n>` appends n assistant message
                    // events in one batched journal.append frame (plus idle),
                    // and nothing else. It sits off every scripted path; the
                    // journal-window hub e2e uses it to push a journal past
                    // the 2000-row / 8MiB tail window before a late attach.
                    if let Some(count) = prompt
                        .strip_prefix("__journal_burst__:")
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        append_n = append_burst(
                            &mut ws,
                            &instance_id,
                            append_n,
                            count,
                            "__journal_burst__",
                        )
                        .await?;
                        append_n =
                            append_native_status(&mut ws, &instance_id, append_n, "idle").await?;
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        continue;
                    }
                    // c-perfaudit: `__perf_transcript__:<events>:<batchesPerSec>`
                    // streams a paced tool/message journal flood (HUB_E2E_PERF=1
                    // only; the perf Playwright project sets it).
                    if perf_flood_enabled()
                        && let Some([events, bps]) = prompt
                            .strip_prefix("__perf_transcript__:")
                            .and_then(perf_parse_parts)
                    {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_n = append_perf_transcript(
                            &mut ws,
                            &instance_id,
                            append_n,
                            events.max(2000),
                            bps.max(1),
                            &mut frame_queue,
                        )
                        .await?;
                        continue;
                    }
                    // c-perfaudit: `__perf_interactions__:<n>` parks n pending
                    // approval cards for the inbox-flood scenario.
                    if perf_flood_enabled()
                        && let Some(count) = prompt
                            .strip_prefix("__perf_interactions__:")
                            .and_then(|tail| tail.parse::<u64>().ok())
                    {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        let mut guard = pending.lock().await;
                        for i in 0..count.max(100) {
                            let iid = InteractionId::new();
                            let mut card = fake_approval(&instance_id, host, iid.as_id().as_str());
                            card["request"]["description"] = json!(format!("perf approval {i}"));
                            guard.insert(iid.as_id().as_str().to_string(), card);
                        }
                        drop(guard);
                        continue;
                    }
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
                    // Two-way: a mode changed in the terminal itself (a bare
                    // shift+tab) folds into the chip as source `unknown` — no
                    // instance.configure goes back, so there is no ping-pong.
                    if let Some(word) = prompt.strip_prefix("__perm__:") {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_n = append_event(
                            &mut ws,
                            &instance_id,
                            append_n,
                            "permission",
                            json!({
                                "effective": {
                                    "mode": word,
                                    "source": "unknown",
                                    "observedAt": "2026-09-16T12:00:00.000Z"
                                },
                                "raw": word
                            }),
                        )
                        .await?;
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
                    // c-mfix round 2: the full phone-chrome combo fixture
                    // (exited resumable instance + live hook/tool/status strip).
                    if prompt == "mfix-chrome-combo" {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_n =
                            append_mfix_chrome_combo(&mut ws, &instance_id, append_n).await?;
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
                    // c-toolfold: a Bash tool the reader watches while it is
                    // running, then settles live with no reload. Proves D-041's
                    // automatic compact fold fires the moment the final result
                    // arrives (not only for reloaded settled history). The
                    // "mcp" variant mounts a settled card with a long qualified
                    // MCP name for the 390px overflow/hit-target assertions.
                    if prompt.contains("toolfold settle") {
                        send_rpc_ok(
                            &mut ws,
                            id,
                            json!({ "ok": true, "instanceId": instance_id }),
                        )
                        .await?;
                        append_n = append_toolfold_settle_scenario(
                            &mut ws,
                            &instance_id,
                            append_n,
                            prompt.contains("mcp"),
                        )
                        .await?;
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
                    // c-cua-media: a computer-use MCP call whose result carries
                    // a screenshot staged through the host-token object route.
                    if cua_screenshot_prompt(prompt) {
                        send_rpc_ok(
                            &mut ws,
                            id,
                            json!({ "ok": true, "instanceId": instance_id }),
                        )
                        .await?;
                        append_n = append_cua_scenario(
                            &mut ws,
                            addr,
                            host_id.as_id().as_str(),
                            &durable_token,
                            &instance_id,
                            prompt,
                            command_id,
                            append_n,
                        )
                        .await?;
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
                    // A prompt containing "hold-working" models an agent that
                    // is STILL in a long turn after the send (composer keeps
                    // projecting working); without the sentinel the fake turn
                    // completes and reports idle, like a harness emitting Stop.
                    let hold_working = prompt.contains("hold-working");
                    let mode = params.get("mode").and_then(Value::as_str);
                    // c-steer: a 插队 ends the running turn before the prompt
                    // runs. Journal the interrupt (origin + reason, mirroring
                    // the Node ledger) and report idle, so the web flushes the
                    // held queue behind it.
                    if mode == Some("steer") {
                        append_n = append_steer_interrupt(
                            &mut ws,
                            &instance_id,
                            append_n,
                            command_id.unwrap_or("unknown"),
                        )
                        .await?;
                    }
                    // A normal turn completes (idle); an explicit queue send
                    // and the hold-working sentinel keep the composer working.
                    let next_status =
                        if mode == Some("queue") || (mode != Some("steer") && hold_working) {
                            "working"
                        } else {
                            "idle"
                        };
                    append_n =
                        append_native_status(&mut ws, &instance_id, append_n, next_status).await?;
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
                            // The real driver turns configure into typed
                            // `/model <id>` bytes; emulate the terminal seeing
                            // them (a native-typed user node) before the
                            // verdict lands.
                            append_n = append_native_user(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("/model {requested_id}"),
                            )
                            .await?;
                            // Listed vs typed: a configure id the session's own
                            // launch catalog offered is "listed"; anything else
                            // (host-fallback rows, free-typed ids) rides the
                            // typed-id fallback path and the verdict decides.
                            let listed = model_catalogs
                                .get(&instance_id)
                                .is_some_and(|ids| ids.iter().any(|id| id == &requested_id));
                            let selection_path = if listed { "listed" } else { "typed" };
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
                                        "observedAt": "2026-09-14T12:00:00.000Z",
                                        // Listed when the id was offered by
                                        // the session's own launch catalog;
                                        // the typed-id case exercises typed.
                                        "selectionPath": selection_path
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
                    // Permission-mode configure. Test-only sentinels mirror the
                    // effort ones (the UI sends real mode ids, never these):
                    //   "__queued__:<mode>"  → permission-queued lifecycle only
                    //   "__degrade__:<mode>" → permission-degraded lifecycle only
                    // A real mode id echoes the closed-loop read-back: the fake
                    // agent cycled the wheel and now reports the effective mode.
                    if let Some(requested) = params.get("permissionMode").and_then(Value::as_str) {
                        if let Some(word) = requested.strip_prefix("__queued__:") {
                            append_n = append_configure_status(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("permission-queued:{word}"),
                            )
                            .await?;
                        } else if let Some(word) = requested.strip_prefix("__degrade__:") {
                            append_n = append_configure_status(
                                &mut ws,
                                &instance_id,
                                append_n,
                                &format!("permission-degraded:{word}:no-status-line"),
                            )
                            .await?;
                        } else {
                            append_n = append_event(
                                &mut ws,
                                &instance_id,
                                append_n,
                                "permission",
                                json!({
                                    "requested": requested,
                                    "effective": {
                                        "mode": requested,
                                        "source": "remuda",
                                        "observedAt": "2026-09-16T12:00:00.000Z"
                                    },
                                    "raw": requested
                                }),
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
                "instance.delete" | "instance.purge" => {
                    // A deleted instance's pending cards can never be answered
                    // again; purge them like the real Node, so they stop
                    // leaking into the shared Hub's approvals queue.
                    pending.lock().await.retain(|_iid, card| {
                        card.get("instanceId").and_then(Value::as_str) != Some(instance_id.as_str())
                    });
                    ttys.remove(&instance_id);
                    send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                }
                "interaction.list" => {
                    // Honour the list filters the real Node applies
                    // (remuda-node interactions::dispatch_rpc); otherwise
                    // pending cards from other instances leak into a filtered
                    // query when many specs share this one fake Node.
                    let want_instance = params.get("instanceId").and_then(Value::as_str);
                    let want_kind = params.get("kind").and_then(Value::as_str);
                    let items: Vec<Value> = pending
                        .lock()
                        .await
                        .values()
                        .filter(|item| {
                            want_instance.is_none_or(|id| {
                                item.get("instanceId").and_then(Value::as_str) == Some(id)
                            }) && want_kind.is_none_or(|kind| {
                                item.get("kind").and_then(Value::as_str) == Some(kind)
                            })
                        })
                        .cloned()
                        .collect();
                    send_rpc_ok(&mut ws, id, json!({ "items": items })).await?;
                    // A scripted terminal answer starts its timer here: the
                    // card has now been listed to a client at least once, so
                    // the spec's "pending after the form is visible" assertion
                    // cannot lose the race against it.
                    let armed: Vec<(String, String)> = {
                        let mut armed = terminal_pending.lock().await;
                        items
                            .iter()
                            .filter_map(|item| item.get("id").and_then(Value::as_str))
                            .filter_map(|iid| {
                                armed
                                    .remove(iid)
                                    .map(|instance| (iid.to_string(), instance))
                            })
                            .collect()
                    };
                    for (iid, terminal_instance) in armed {
                        let tx = close_tx.clone();
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
                        && let Some(answered_instance) =
                            card.get("instanceId").and_then(Value::as_str)
                    {
                        if card.get("kind").and_then(Value::as_str) == Some("question") {
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
                        }
                        // c-steer: any human decision ends the dialog; model
                        // the harness completing the turn and report idle so
                        // the composer's held messages flush.
                        append_n =
                            append_native_status(&mut ws, answered_instance, append_n, "idle")
                                .await?;
                    }
                }
                "workspace.scm.status" | "workspace.scm.diff" | "workspace.scm.file" => {
                    let result = g2_scm_answer(method, &params);
                    send_rpc_ok(&mut ws, id, result).await?;
                }
                "host.files.list" | "host.files.read" => {
                    match host_files_answer(method, &params, addr, host, &durable_token).await {
                        Ok(result) => send_rpc_ok(&mut ws, id, result).await?,
                        Err(error) => send_rpc_error(&mut ws, id, &error.to_string()).await?,
                    }
                }
                "subagent.transcript" => {
                    let agent_id = params.get("agentId").and_then(Value::as_str).unwrap_or("");
                    send_rpc_ok(&mut ws, id, drill_subagent_answer(agent_id)).await?;
                }
                "tty.screen" => {
                    // ONE arm with a per-instance condition chain. Adding a
                    // screen case means inserting a branch here, never a second
                    // match arm (a later literal arm would be unreachable).
                    //   1. an instance that opted in with the `screen-read`
                    //      create sentinel replays its cooked buffer;
                    //   2. default: exactly the old answer (ok, no lines), so
                    //      every other spec's list rows keep journal text.
                    // (The c-mobilenew e2e gate parks the frame earlier in the
                    // loop when its switch file exists, so saturated reads
                    // never reach this.)
                    let limit = params.get("lines").and_then(Value::as_u64).unwrap_or(80) as usize;
                    if screen_enabled.contains(&instance_id)
                        && let Some(tty) = ttys.get(&instance_id)
                    {
                        let lines = String::from_utf8_lossy(&tty.screen)
                            .split(['\r', '\n'])
                            .filter(|line| !line.is_empty())
                            .rev()
                            .take(limit)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .map(str::to_owned)
                            .collect::<Vec<_>>();
                        send_rpc_ok(&mut ws, id, json!({ "lines": lines })).await?;
                    } else {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                    }
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
                    //
                    // The browser's per-row remote controls send logical key
                    // names (`keys: ["esc"]`, mapped the same way the real Node
                    // maps them in its `tty.write` arm) instead of dataBase64;
                    // translate them here so the row-overflow e2e can prove the
                    // key reached the fake harness.
                    let mut bytes = params
                        .get("dataBase64")
                        .and_then(Value::as_str)
                        .and_then(|raw| base64::engine::general_purpose::STANDARD.decode(raw).ok())
                        .unwrap_or_default();
                    if bytes.is_empty()
                        && let Some(keys) = params.get("keys").and_then(Value::as_array)
                    {
                        let names = keys
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>();
                        bytes = remuda_driver::logical_keys_to_bytes(&names);
                    }
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
                    // c-perfaudit: `__perf_tty__:<lines>:<perFrame>:<fps>`
                    // floods the raw TTY stream (HUB_E2E_PERF=1 only; the perf
                    // Playwright project sets it and the spec skips without it).
                    let perf_tty = if perf_flood_enabled() {
                        submitted
                            .as_deref()
                            .and_then(|line| line.strip_prefix("__perf_tty__:"))
                            .and_then(perf_parse_parts::<3>)
                    } else {
                        None
                    };
                    if let Some([lines, per_frame, fps]) = perf_tty {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        append_perf_tty_flood(
                            &mut ws,
                            &instance_id,
                            tty,
                            lines.max(5000),
                            per_frame.max(1),
                            fps.max(1),
                            &mut frame_queue,
                        )
                        .await?;
                        continue;
                    }
                    // `TTYNODE_RESTART` typed into a session makes the fake
                    // Node restart: ack the write, drop the socket, reconnect
                    // under a new epoch, and re-announce an inventory that no
                    // longer holds the session the operator was looking at.
                    //
                    // Only that one is dropped, and the harness's own maps are
                    // left alone: this is a *shared* single-node harness, and
                    // a restart that wiped every other spec's live session
                    // would fail them for reasons that have nothing to do with
                    // their subject. What matters to the Hub is the hello's
                    // inventory, not what this fixture believes.
                    if submitted.as_deref()
                        == Some(std::str::from_utf8(NODE_RESTART_SENTINEL).unwrap_or_default())
                    {
                        send_rpc_ok(&mut ws, id, json!({ "ok": true })).await?;
                        let mut survivors: Vec<String> = ttys
                            .keys()
                            .chain(claude_ptys.iter())
                            .filter(|id| id.as_str() != instance_id.as_str())
                            .cloned()
                            .collect();
                        survivors.sort();
                        survivors.dedup();
                        let _ = ws.close(None).await;
                        // node_restart_reconnect bumps node_epoch itself.
                        ws = node_restart_reconnect(
                            addr,
                            &durable_token,
                            &host_id,
                            &workspaces,
                            &mut node_epoch,
                            survivors,
                            &mut frame_queue,
                        )
                        .await?;
                        continue;
                    }
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
                "worker.provision" => {
                    // Dispatch (api-route.hub.spec): provision a synthetic
                    // worktree under the announced e2e root; no real git.
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("x");
                    let branch = params
                        .get("branch")
                        .and_then(Value::as_str)
                        .unwrap_or("wt/x/work");
                    send_rpc_ok(
                        &mut ws,
                        id,
                        json!({
                            "name": name,
                            "branch": branch,
                            "startPoint": "origin/main",
                            "worktreePath": format!("/tmp/remuda-e2e/wt/{name}"),
                            "targetDir": format!("/tmp/remuda-e2e/target/{name}"),
                        }),
                    )
                    .await?;
                }
                "worker.remove" => {
                    send_rpc_ok(
                        &mut ws,
                        id,
                        json!({
                            "name": params.get("name").and_then(Value::as_str).unwrap_or("x"),
                            "worktreeRemoved": true,
                            "targetRemoved": true,
                            "reclaimedBytes": "2048",
                        }),
                    )
                    .await?;
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

/// Typing this line (followed by Enter) makes the fake Node *restart*: it
/// reconnects under a new `nodeEpoch` announcing an inventory that no longer
/// contains the other live instances, exactly as `hubnode_codec::stdio_hello_params`
/// now reports after a real Node's sweep.
///
/// The inventory is the whole point: before this, a hello carried no
/// `instances` key at all and the Hub had an epoch change with nothing to
/// diff, so the rows of every process that died with the previous Node stayed
/// `running` and kept holding placement slots.
const NODE_RESTART_SENTINEL: &[u8] = b"TTYNODE_RESTART";
/// c-ghostbadge round 2: an `instance.send` prompt that restarts the fake
/// Node under a new epoch re-announcing every other instance as a survivor,
/// so reconcile settles the addressed instance exited.
const GHOST_RESTART_SENTINEL: &str = "GHOSTNODE_RESTART";

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

// ── t-bind fake worktree layer (HUB_E2E_TASK_BIND=1 only) ──────────────────
//
// The default fake Node holds no git repository, so the worktree lease RPCs
// are modelled in memory behind an explicit trigger; every other spec keeps
// the previous "unknown method" behaviour. The model mirrors the real Node
// contract (crates/remuda-node/src/worktree_pool.rs): reuse attaches with a
// refcount and queues on sharing, pool allocates `<pool>-s<n>` slots with a
// per-task branch, a dirty tree refuses, and a full pool defers.

fn task_bind_enabled() -> bool {
    std::env::var("HUB_E2E_TASK_BIND").as_deref() == Ok("1")
}

// ── c-perfaudit high-rate flood triggers (HUB_E2E_PERF=1 only) ────────────
//
// The performance scenarios (web/tests/perf/scenarios.perf.ts via
// playwright.perf.config.ts) need event streams a normal scripted turn cannot
// produce: thousands of journal events at a fixed batch rate, a terminal
// frame flood, and a hundred pending interactions. The sentinels are inert on
// every other run (they fall through to the normal echo paths), and the Playwright
// spec test.skips when this trigger is absent.
fn perf_flood_enabled() -> bool {
    std::env::var("HUB_E2E_PERF").as_deref() == Ok("1")
}

/// Parse a `a:b[:c]` sentinel tail into its numeric parts.
fn perf_parse_parts<const K: usize>(tail: &str) -> Option<[u64; K]> {
    let parts: Vec<&str> = tail.split(':').collect();
    if parts.len() != K {
        return None;
    }
    let mut out = [0u64; K];
    for (slot, part) in out.iter_mut().zip(parts) {
        *slot = part.parse().ok()?;
    }
    Some(out)
}

// ── t-project-switcher second fake host (HUB_E2E_PROJECT_SWITCHER=1 only) ──
//
// Off by default: a default full-suite run enrolls exactly the primary fake
// node, so every other spec sees an unchanged host directory. With the
// trigger set, the primary node gains one /tmp/remuda-project-a branded
// workspace and a second enrolled node (`e2e-project-host-b`) announces
// /tmp/remuda-project-b, giving the project-switcher spec members on two
// hosts without merging Space keys (D-024).
fn project_switcher_enabled() -> bool {
    std::env::var("HUB_E2E_PROJECT_SWITCHER").as_deref() == Ok("1")
}

/// One in-memory catalog row the binding fake hands out via worktree.list.
#[derive(Clone)]
struct BindWorktree {
    name: String,
    path: String,
    branch: String,
    pooled: bool,
    leased_by: Vec<String>,
}

impl BindWorktree {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path,
            "branch": self.branch,
            "base": "main",
            "state": if self.leased_by.is_empty() { "parked" } else { "leased" },
            "pooled": self.pooled,
            "leasedBy": self.leased_by,
        })
    }
}

/// Bind lease state held for the fake Node's lifetime.
#[derive(Default)]
struct BindLeaseState {
    /// Catalog keyed by worktree name. Seeded with one standalone reuse dir.
    catalog: HashMap<String, BindWorktree>,
    /// Next slot number per pool name.
    slot_next: HashMap<String, usize>,
}

impl BindLeaseState {
    fn seeded() -> Self {
        let mut state = Self::default();
        state.catalog.insert(
            "agent-one".into(),
            BindWorktree {
                name: "agent-one".into(),
                path: "/tmp/remuda-bind/remuda-wt/agent-one".into(),
                branch: "wt/agent-one/work".into(),
                pooled: false,
                leased_by: Vec::new(),
            },
        );
        // Separate standalone dirs so serial specs sharing one hub do not
        // count each other's leases on the same key.
        for name in ["agent-two", "agent-three", "agent-race"] {
            state.catalog.insert(
                name.into(),
                BindWorktree {
                    name: name.into(),
                    path: format!("/tmp/remuda-bind/remuda-wt/{name}"),
                    branch: format!("wt/{name}/work"),
                    pooled: false,
                    leased_by: Vec::new(),
                },
            );
        }
        state
    }

    fn list(&self) -> Value {
        let mut items: Vec<Value> = self.catalog.values().map(BindWorktree::to_json).collect();
        items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!({ "workspaceRoot": "/tmp/remuda-bind", "items": items, "nextCursor": null })
    }

    /// Model `worktree.lease`. Returns Ok(result) or Err(message).
    fn lease(&mut self, params: &Value) -> std::result::Result<Value, String> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let task = params
            .get("taskId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // reuse-to-root.
        if name == "." {
            return Ok(json!({
                "name": ".",
                "path": "/tmp/remuda-bind",
                "branch": "main",
                "base": Value::Null,
                "mode": "reuse",
                "state": "leased",
                "refcount": 1,
                "warm": true,
                "queued": false,
                "dirKey": ".",
            }));
        }
        // Refusal fixtures: never reroute.
        if name == "dirty" {
            return Err("worktree dirty-slot is dirty; refusing to lease it".into());
        }
        if name == "fullpool" {
            return Ok(json!({
                "deferred": true,
                "code": "SUPPLY_DEFERRED",
                "reason": "worktree pool 'fullpool' is full and no clean parked slot is available",
                "poolSize": 4,
            }));
        }
        // Existing catalog row: standalone reuse dir or an already-allocated
        // slot reached by its slot name.
        if let Some(row) = self.catalog.get_mut(name) {
            let already = row.leased_by.contains(&task);
            let queued = !row.leased_by.is_empty() && !already;
            if !already {
                row.leased_by.push(task.clone());
            }
            let refcount = row.leased_by.len();
            let mode = if row.pooled { "pool" } else { "reuse" };
            let mut payload = json!({
                "name": row.name,
                "path": row.path,
                "branch": row.branch,
                "base": Value::Null,
                "mode": mode,
                "state": "leased",
                "refcount": refcount,
                "warm": true,
                "queued": queued,
                "dirKey": row.name,
            });
            if queued {
                payload["blocked"] = json!({
                    "reason": "dir-busy: directory is held by another attached task; queued for serial reuse"
                });
            }
            return Ok(payload);
        }
        // Otherwise allocate a pool slot `<name>-s<n>` with a per-task branch.
        let number = self.slot_next.remove(name).unwrap_or(0) + 1;
        self.slot_next.insert(name.to_string(), number);
        let slot = format!("{name}-s{number}");
        let task_slug = task.replace('_', "-");
        let branch = format!("wt/{slot}/{task_slug}");
        let path = format!("/tmp/remuda-bind/remuda-wt/{slot}");
        self.catalog.insert(
            slot.clone(),
            BindWorktree {
                name: slot.clone(),
                path: path.clone(),
                branch: branch.clone(),
                pooled: true,
                leased_by: vec![task],
            },
        );
        Ok(json!({
            "name": slot,
            "path": path,
            "branch": branch,
            "baseOid": "0123456789abcdef0123456789abcdef01234567",
            "mode": "pool",
            "state": "leased",
            "refcount": 1,
            "warm": false,
            "queued": false,
            "dirKey": slot,
        }))
    }
}

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

/// c-mhome: an entity `exited` lifecycle carrying the native session id and
/// the last error text. The Hub folds both onto the instance row; the error
/// is what the phone home renders in the row body ("errors as body"), while
/// the session id keeps D-026 resume available.
async fn append_instance_exit(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    session_id: &str,
    last_error: &str,
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
                        "type": "entity", "entityType": "instance", "state": "exited",
                        "entity": {
                            "nativeRef": { "sessionId": { "state": "known", "value": session_id } },
                            "lastError": last_error
                        }
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

/// c-mfix round 2: the exact phone chrome combination the owner hit while the
/// soft keyboard was up: an exited (but resumable) instance whose last turn
/// still paints a full live status strip — a running AskUserQuestion tool on
/// the hook tier (its last hook record is 30 s old, so the strip computes a
/// stalled-hook 「通道静默」 note), a fresh screen spinner keeping the turn
/// decision in `working` (so Esc 打断 renders), and an 858-output-token usage
/// snapshot. The web spec asserts the compact keyboard band still shows the
/// composer and a >=40% transcript.
async fn append_mfix_chrome_combo(ws: &mut NodeWs, instance_id: &str, mut n: u64) -> Result<u64> {
    let session_id = format!("mfix-chrome-{}", uuid::Uuid::now_v7());
    let rfc3339 = |t: time::OffsetDateTime| {
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
    };
    // Backdated 30 s: older than the hook tier's 3x cadence stall budget.
    let stale_at = rfc3339(time::OffsetDateTime::now_utc() - time::Duration::seconds(30));
    let now_at = rfc3339(time::OffsetDateTime::now_utc());

    let entity = |state: &str, at: &str| {
        json!({
            "kind": "lifecycle",
            "observedAt": at,
            "payload": {
                "type": "entity", "entityType": "instance", "state": state,
                "entity": {
                    "nativeRef": {
                        "sessionId": { "state": "known", "value": session_id },
                        "signalTier": "hook"
                    }
                }
            }
        })
    };
    n = append_full_event(ws, instance_id, n, entity("ready", &now_at)).await?;
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "message",
            "completeness": "structured",
            "observedAt": now_at,
            "payload": { "role": "user", "text": "mfix chrome combo 最新消息", "origin": "human" }
        }),
    )
    .await?;
    // c-mfix round 4: a long backlog so the transcript OVERFLOWS the keyboard
    // band — pinning must keep the tail on screen when the scroller shrinks.
    // Twelve completed assistant turns, each a few lines tall.
    for i in 1..=12 {
        n = append_full_event(
            ws,
            instance_id,
            n,
            json!({
                "kind": "message",
                "completeness": "structured",
                "observedAt": stale_at,
                "payload": {
                    "role": "assistant",
                    "text": format!("历史回合 {i}：这是一段足够高的多行输出，\n用来把 transcript 撑出键盘 band，\n确保滚动口发生溢出。"),
                    "origin": "assistant"
                }
            }),
        )
        .await?;
    }
    // Prior-turn usage lands BEFORE the currently running tool, so the
    // running AskUserQuestion card is the transcript's final (latest) row.
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "usage",
            "completeness": "structured",
            "observedAt": stale_at,
            "source": { "channel": "hook" },
            "payload": {
                "usageId": "obj_mfix_usage",
                "scope": "turn",
                "scopeId": "obj_mfix_run",
                "mode": "snapshot",
                "metricRevision": "1",
                "inputTokens": wf_known(json!("12000")),
                "inputAccounting": "uncached",
                "outputTokens": wf_known(json!("858")),
                "reasoningTokens": wf_unknown(),
                "cacheReadTokens": wf_known(json!("0")),
                "cacheWriteTokens": wf_known(json!("0")),
                "totalTokens": wf_known(json!("12858")),
                "cost": { "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] },
                "accounting": "estimated",
                "nativeFieldsRef": null
            }
        }),
    )
    .await?;
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "tool_call",
            "completeness": "structured",
            "observedAt": stale_at,
            "source": { "channel": "hook" },
            "payload": {
                "nodeId": "obj_mfix_ask",
                "revision": "1",
                "operation": "open",
                "baseRevision": null,
                "toolCallId": "obj_mfix_ask",
                "parentToolCallId": null,
                "toolName": wf_known(json!("AskUserQuestion")),
                "displayTitle": wf_known(json!("AskUserQuestion")),
                "category": "other",
                "input": wf_known(json!({ "questions": [] })),
                "inputTextDelta": null,
                "state": "running",
                "executor": wf_unknown()
            }
        }),
    )
    .await?;
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "lifecycle",
            "completeness": "structured",
            "observedAt": stale_at,
            "source": { "channel": "hook" },
            "payload": {
                "type": "native",
                "nativeName": "turn.phase",
                "relatedIds": {
                    "phase": "tool-started",
                    "tier": "hook",
                    "toolCallId": "obj_mfix_ask",
                    "toolName": "AskUserQuestion",
                    "since": stale_at
                }
            }
        }),
    )
    .await?;

    // The fresh screen spinner keeps the decision `working` even though the
    // hook tier is stalled (Esc 打断 must render).
    n = append_full_event(
        ws,
        instance_id,
        n,
        json!({
            "kind": "lifecycle",
            "completeness": "structured",
            "observedAt": now_at,
            "source": { "channel": "screen" },
            "payload": {
                "type": "native",
                "nativeName": "live.status",
                "relatedIds": {
                    "liveStatus": "1",
                    "verb": "running PreToolUse hooks",
                    "interruptible": "1",
                    "since": now_at
                }
            }
        }),
    )
    .await?;
    append_full_event(ws, instance_id, n, entity("exited", &now_at)).await
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

/// Append `count` assistant messages in ONE batched `journal.append` frame.
///
/// The Hub folds the batch into bounded writer chunks and assigns contiguous
/// seqs; the single ack carries the final seq. Used by the journal-window
/// e2e to push a journal past the bounded tail window quickly — one frame,
/// not thousands of socket round trips.
async fn append_burst(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    count: u64,
    marker: &str,
) -> Result<u64> {
    let last_seq = n + count;
    let events: Vec<Value> = (1..=count)
        .map(|i| {
            json!({
                "kind": "message",
                "completeness": "structured",
                "payload": { "role": "assistant", "text": format!("{marker} event {}", n + i) }
            })
        })
        .collect();
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": format!("j{last_seq}"),
            "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "events": events,
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    Ok(last_seq)
}

/// c-perfaudit: pump Hub->Node frames already buffered on the socket into the
/// stashed-frame queue so a long paced flood cannot let the TCP buffer (or a
/// pending RPC) grow unbounded. The parked frames are reprocessed by the main
/// loop the moment the flood returns.
async fn perf_pump_frames(ws: &mut NodeWs, queue: &mut std::collections::VecDeque<String>) {
    while let Ok(Some(Ok(Message::Text(text)))) =
        tokio::time::timeout(Duration::from_millis(2), ws.next()).await
    {
        queue.push_back(text.to_string());
    }
}

/// c-perfaudit scenario A: stream `events_target` journal events (rounded up
/// to the batch size) at `batches_per_sec` frames/sec. Each 10-event batch is
/// 3 tool call/result pairs plus 4 assistant messages, so 2000 events carry
/// 600 tools — above the 500-tool scenario floor. The caller acks the command
/// RPC first; this only streams (and ends the turn idle).
///
/// Pacing is anchored to wall clock and does NOT wait on the Hub's per-batch
/// ack — ack round-trip latency would cap the send rate below the target.
/// Inbound frames are pumped into the stashed queue while streaming; a final
/// ack barrier (plus the trailing idle append, also acked) guarantees the Hub
/// durably processed every batch before this returns.
async fn append_perf_transcript(
    ws: &mut NodeWs,
    instance_id: &str,
    mut n: u64,
    events_target: u64,
    batches_per_sec: u64,
    queue: &mut std::collections::VecDeque<String>,
) -> Result<u64> {
    const BATCH: u64 = 10;
    n = append_native_status(ws, instance_id, n, "working").await?;
    wait_frame_ack(ws, queue, &format!("j{n}")).await?;
    let batches = events_target.max(BATCH).div_ceil(BATCH);
    let period = Duration::from_nanos(1_000_000_000 / batches_per_sec.clamp(1, 1000));
    let flood_started = tokio::time::Instant::now();
    for b in 0..batches {
        let mut events: Vec<Value> = Vec::with_capacity(BATCH as usize);
        for k in 0..BATCH {
            let global = b * BATCH + k;
            if k % 4 < 2 {
                // k pairs (0,1),(4,5),(8,9): three tools per batch.
                let tool_no = b * 3 + k / 4;
                let tool_id = format!("perf-tcall-{tool_no}");
                if k % 2 == 0 {
                    events.push(json!({
                        "kind": "tool_call",
                        "payload": {
                            "nodeId": tool_id,
                            "revision": "1",
                            "operation": "open",
                            "baseRevision": null,
                            "toolCallId": tool_id,
                            "parentToolCallId": null,
                            "toolName": wf_known(json!("Bash")),
                            "displayTitle": wf_known(json!("Bash")),
                            "category": "shell",
                            "input": wf_known(json!({ "command": format!("echo perf {tool_no}") })),
                            "inputTextDelta": null,
                            "state": "running",
                            "executor": wf_unknown(),
                        }
                    }));
                } else {
                    events.push(json!({
                        "kind": "tool_result",
                        "payload": {
                            "nodeId": tool_id,
                            "revision": "2",
                            "operation": "close",
                            "baseRevision": "1",
                            "toolCallId": tool_id,
                            "stage": "final",
                            "outcome": "succeeded",
                            "blocks": [{ "type": "text", "text": format!("perf ok {tool_no}\n") }],
                            "structuredResult": wf_known(json!({ "stdout": format!("perf ok {tool_no}\n") })),
                            "exitCode": wf_known(json!(0)),
                            "changes": [],
                        }
                    }));
                }
            } else {
                events.push(json!({
                    "kind": "message",
                    "completeness": "structured",
                    "payload": { "role": "assistant", "text": format!("perf transcript event {global}") }
                }));
            }
        }
        n += BATCH;
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": format!("j{n}"),
                "method": "journal.append",
                "params": { "instanceId": instance_id, "events": events }
            })
            .to_string()
            .into(),
        ))
        .await?;
        // Drain any acks/RPCs the Hub sent back without blocking the cadence…
        perf_pump_frames(ws, queue).await;
        // …then sleep only until this batch's scheduled tick.
        let target = flood_started + period * (b + 1) as u32;
        let now = tokio::time::Instant::now();
        if now < target {
            tokio::time::sleep(target - now).await;
        }
    }
    // Barrier: the last batch's ack proves every earlier append was received
    // and durably processed before the turn ends idle.
    wait_frame_ack(ws, queue, &format!("j{n}")).await?;
    n = append_native_status(ws, instance_id, n, "idle").await?;
    wait_frame_ack(ws, queue, &format!("j{n}")).await?;
    Ok(n)
}

/// c-perfaudit scenario B: push `lines_target` terminal output lines as
/// `tty.frame` notifications, `per_frame` lines per frame at `frames_per_sec`,
/// while continuing to pump inbound frames. The cooked screen is retained as a
/// bounded ring like a real PTY.
async fn append_perf_tty_flood(
    ws: &mut NodeWs,
    instance_id: &str,
    tty: &mut TtyFake,
    lines_target: u64,
    per_frame: u64,
    frames_per_sec: u64,
    queue: &mut std::collections::VecDeque<String>,
) -> Result<()> {
    let per_frame = per_frame.clamp(1, 500);
    let period = Duration::from_nanos(1_000_000_000 / frames_per_sec.clamp(1, 1000));
    let mut sent = 0u64;
    let mut tick = 0u64;
    while sent < lines_target {
        let started = tokio::time::Instant::now();
        let mut chunk = String::new();
        for _ in 0..per_frame {
            if sent >= lines_target {
                break;
            }
            sent += 1;
            chunk.push_str(&format!("perf flood line {sent:05}\r\n"));
        }
        tty.screen.extend_from_slice(chunk.as_bytes());
        if tty.screen.len() > 65_536 {
            let cutoff = tty.screen.len() - 65_536;
            tty.screen.drain(..cutoff);
        }
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "tty.frame",
                "params": {
                    "instanceId": instance_id,
                    "streamId": tty.stream_id,
                    "dataBase64": base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes()),
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
        tick += 1;
        // A non-blocking pump most ticks; an occasional 2 ms wait lets an
        // in-flight tty.resize (or any Hub RPC) land and get parked.
        if tick.is_multiple_of(25) {
            perf_pump_frames(ws, queue).await;
        }
        let elapsed = started.elapsed();
        if elapsed < period {
            tokio::time::sleep(period - elapsed).await;
        }
    }
    perf_pump_frames(ws, queue).await;
    Ok(())
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

/// c-steer: journal the turn a 插队 interrupts, mirroring the Node's
/// `turn-interrupted` native lifecycle (`origin` + `reason` + commandId in
/// `relatedIds`), so a transcript reader sees WHY the previous turn ended.
async fn append_steer_interrupt(
    ws: &mut NodeWs,
    instance_id: &str,
    n: u64,
    command_id: &str,
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
                        "topic": "diagnostic",
                        "nativeName": "turn-interrupted",
                        "status": "esc-dispatched",
                        "severity": "info",
                        "relatedIds": {
                            "origin": "human",
                            "reason": "user-steer",
                            "commandId": command_id
                        }
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
        "live" => "live",
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

/// c-wfcard: RFC3339-ms timestamp at `now + offset_ms`, as a JSON string.
/// The workflow fixtures pin real `launchedAt` / `startedAt` / `endedAt` /
/// `lastProgressAt` values this way so the live card's clocks render instead
/// of the em dash (the hand-format shape the wire Timestamp requires).
fn wf_timestamp(offset_ms: i128) -> Value {
    let ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000 + offset_ms;
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000)
        .expect("millisecond in range");
    json!(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond(),
    ))
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
    started_at: Value,
    ended_at: Value,
    last_progress_at: Value,
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
        "startedAt": started_at,
        "endedAt": ended_at,
        "lastProgressAt": last_progress_at,
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

/// c-toolfold: one Bash tool observed running, then settled live. The call
/// frame goes out first, the scenario pauses long enough for a follower to
/// render the running card, then the final result lands in a later frame so
/// the web's automatic compact fold can be proven without a reload. With
/// `long_mcp`, emit a settled card carrying an over-40-char qualified MCP
/// name instead (no running gap): the 390px overflow/hit-target case.
async fn append_toolfold_settle_scenario(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    instance_id: &str,
    mut n: u64,
    long_mcp: bool,
) -> Result<u64> {
    let tool_id = "obj_toolfold_settle";
    if long_mcp {
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
                "toolName": wf_known(json!("mcp__remuda-very-long-integration-server__search_files_everywhere")),
                "displayTitle": wf_known(json!("mcp__remuda-very-long-integration-server__search_files_everywhere")),
                "category": "other",
                "input": wf_known(json!({ "query": "toolfold long mcp name" })),
                "inputTextDelta": null,
                "state": "running",
                "executor": wf_unknown(),
            }),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "tool_result",
            json!({
                "nodeId": tool_id,
                "revision": "2",
                "operation": "close",
                "baseRevision": "1",
                "toolCallId": tool_id,
                "stage": "final",
                "outcome": "succeeded",
                "blocks": [{ "type": "text", "text": "ok\n" }],
                "structuredResult": wf_known(json!({ "matches": [] })),
                "exitCode": wf_known(json!(0)),
                "changes": [],
            }),
        )
        .await?;
        n = append_journal(ws, instance_id, n, "assistant", "toolfold mcp 已结束").await?;
        return append_native_status(ws, instance_id, n, "idle").await;
    }
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
            "toolName": wf_known(json!("Bash")),
            "displayTitle": wf_known(json!("Bash")),
            "category": "shell",
            "input": wf_known(json!({ "command": "echo toolfold-live-settle" })),
            "inputTextDelta": null,
            "state": "running",
            "executor": wf_unknown(),
        }),
    )
    .await?;
    // Give the follower time to paint the running row; the hub suite is
    // serial, so this only lengthens this one scripted turn.
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    n = append_event(
        ws,
        instance_id,
        n,
        "tool_result",
        json!({
            "nodeId": tool_id,
            "revision": "2",
            "operation": "close",
            "baseRevision": "1",
            "toolCallId": tool_id,
            "stage": "final",
            "outcome": "succeeded",
            "blocks": [{ "type": "text", "text": "settled live\n" }],
            "structuredResult": wf_known(json!({ "stdout": "settled live\n" })),
            "exitCode": wf_known(json!(0)),
            "changes": [],
        }),
    )
    .await?;
    // A single tool with no thought stays outside the compact summary fold
    // (needs >= 2 tools/thoughts), so the card remains directly addressable.
    n = append_journal(ws, instance_id, n, "assistant", "toolfold 已结束").await?;
    append_native_status(ws, instance_id, n, "idle").await
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
    // c-wfcard: run launch instant; running fixtures are 222 s old (matching
    // the pinned elapsedMs), terminal/legacy fixtures anchor at "now".
    let launched = if kind == "demo-running" || kind == "live" {
        wf_timestamp(-222_000)
    } else {
        wf_timestamp(0)
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

    let run = |state: &str,
               revision: u64,
               totals: Value,
               live: Value,
               note: Option<&str>,
               launched_at: Value| {
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
            "launchedAt": launched_at,
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
                launched.clone(),
            ),
        )
        .await?;
        return Ok(n);
    }

    if kind == "drill" {
        return append_drill_scenario(ws, instance_id, n, &workflow_id, &tool_id, &phase, &member)
            .await;
    }

    if kind == "live" {
        // c-wfcard: same live run as demo-running, but the assistant turn is
        // closed (one ordinary Bash tool + a thought behind it), so dismissal
        // can be proven to fold the card into the compact summary row.
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
                launched.clone(),
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
                wf_timestamp(-222_000),
                wf_timestamp(-114_000),
                wf_timestamp(-114_000),
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
                wf_timestamp(-62_000),
                Value::Null,
                wf_timestamp(-2_000),
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
                Value::Null,
                Value::Null,
                Value::Null,
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
                Value::Null,
                Value::Null,
                Value::Null,
            ),
        )
        .await?;
        // One ordinary (successful) Bash tool in the same turn: it joins the
        // compact fold while the undismissed workflow row stays outside.
        let bash_id = format!("obj_bash_{tag}");
        n = append_event(
            ws,
            instance_id,
            n,
            "tool_call",
            json!({
                "nodeId": bash_id,
                "revision": "1",
                "operation": "open",
                "baseRevision": null,
                "toolCallId": bash_id,
                "parentToolCallId": null,
                "toolName": wf_known(json!("Bash")),
                "displayTitle": wf_known(json!("Bash")),
                "category": "shell",
                "input": wf_known(json!({ "command": "echo workflow-running" })),
                "inputTextDelta": null,
                "state": "running",
                "executor": wf_unknown(),
            }),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "tool_result",
            json!({
                "nodeId": bash_id,
                "revision": "2",
                "operation": "close",
                "baseRevision": "1",
                "toolCallId": bash_id,
                "stage": "final",
                "outcome": "succeeded",
                "blocks": [{ "type": "text", "text": "ok\n" }],
                "structuredResult": wf_known(json!({ "stdout": "ok\n" })),
                "exitCode": wf_known(json!(0)),
                "changes": [],
            }),
        )
        .await?;
        n = append_event(
            ws,
            instance_id,
            n,
            "thought",
            json!({
                "nodeId": format!("obj_thought_{tag}"),
                "revision": "1",
                "operation": "close",
                "baseRevision": null,
                "thoughtId": format!("obj_thought_{tag}"),
                "representation": "summary",
                "text": "plan the workflow run",
                "partIndex": 0,
                "status": "complete",
            }),
        )
        .await?;
        n = append_journal(ws, instance_id, n, "assistant", "workflow 在跑").await?;
        return Ok(n);
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
                wf_timestamp(-122_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
                wf_timestamp(-80_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
                wf_timestamp(-65_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
                launched.clone(),
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
                launched.clone(),
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
                wf_timestamp(-222_000),
                wf_timestamp(-114_000),
                wf_timestamp(-114_000),
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
                wf_timestamp(-62_000),
                Value::Null,
                wf_timestamp(-2_000),
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
                Value::Null,
                Value::Null,
                Value::Null,
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
                Value::Null,
                Value::Null,
                Value::Null,
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
                wf_timestamp(-122_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
                wf_timestamp(-80_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
                wf_timestamp(-65_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
                launched.clone(),
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
                launched.clone(),
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
                    if i == 0 {
                        wf_timestamp(-31_000)
                    } else {
                        Value::Null
                    },
                    Value::Null,
                    if i == 0 {
                        wf_timestamp(-1_000)
                    } else {
                        Value::Null
                    },
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
                    wf_timestamp(-75_000),
                    wf_timestamp(0),
                    wf_timestamp(0),
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
                launched.clone(),
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
            launched.clone(),
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
                wf_timestamp(-99_000),
                wf_timestamp(0),
                wf_timestamp(0),
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
            wf_timestamp(-72_000),
            wf_timestamp(0),
            wf_timestamp(0),
        ),
    )
    .await?;
    Ok(n)
}

/// c-cua-media: the screenshot sentinel — a prompt with this prefix drives the
/// synthetic computer-use tool result instead of the echo path.
fn cua_screenshot_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with("cua screenshot")
}

/// A fixed 240x240 synthetic PNG (a red→blue gradient band, 1622 bytes).
/// Never a real desktop capture (`codex-cua.md` §6.4).
fn cua_png_base64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(cua_png_bytes())
}

fn cua_png_bytes() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(CUA_PNG_240_B64)
        .expect("synthetic png fixture")
}

/// Base64 of the 240x240 synthetic fixture.
const CUA_PNG_240_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAPAAAADwCAIAAACxN37FAAAGHUlEQVR42u3SBxMIBAAGUJJsl1EUulRGqCtb6MqqiCMUX+NaKkqHkIiTNMXJ3ofKTKepxUXKKhcZyTijZOWMSqWu+h+9n/DuXoEC+adg/j4nfxXK2XPzZ+H8cV5+L5IzRfNbsfxaPL+UyOmSOVUqJ0vnxPk5XiY/l82xcjlaPkcuyOELc6hCfqqYgxflx4vzQ6UcqJz9VbLvkuy9NHuqZvdl2XV5dl6R76tlR/V8VyPba2bbldlaK1tq59s62XxVNl2db67Jxmvzdd18VS8b6md9g6xrmLWNsqZxvmySL67L6qb5vFlWNc/K6/PZDVlxY5a3yKct80mrfNw6H7XJhzdl2c354Ja83zbvtcu7t+ad9nm7Q5Z2zFudsuS2vNk5i7tkUdcsvD0L7sj8bpnXPW8kr9+Z1+7K3Lsz557Mvjez7svM+zPjgUx/MNN6ZOpDmfJwJj+SST0zsVcmPJrxj2Vc77z6eMb2yZi+Gd0vrzyRUf3z8oC8NDAvPpkXBuX5p/Lc4IwckmefzoiheWZYhg/PsGcydESefjZDRmbwc3nq+Qx6IU++mIEvZcDL6T8qT7ySfqPTd0z6jM3jr6b3uDw2Po9OSK+J6Tkpj0zOw1Py0NT0mJYHp+eBGbl/Zu6blXtn5545uXtu7notd76e/0jd56Xb/NyxILcvTNdF6bI4nd/MbUvS6a10XJoOb6f9O7n13bR7L23fzy0f5OZluenDtPkorT9Oq0/S8tO0WJ4bV+SGz3L9yjRflWafp+nqXPdFmnyZxmvSaG0arkuD9am/IfW+St2vc+3GXPNNrt6UqzanzrepvSW1tubKbam5PTW+S/UdqfZ9rtiZy3flst2puieX7s0l+1JlfyofSKUfcvGPuehgKv6UCody4eFccCTlj6bcsZT9OWWO5/wTKX0ypU6l5OmU+CXFf02x31L0TIr8nvP+SOE/c+7ZFPor5/ydgv+kQAGhhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFlpooYUWWmihhRZaaKGFFvr/EvpfXjJrkxF8WxAAAAAASUVORK5CYII=";

/// The fake-node stager the *real* transcript mapper calls when it folds the
/// native image item. Like the production Node, it posts to the host-token
/// route asking for typed image storage. The fold runs on the tokio blocking
/// pool (which still carries runtime context), so the upload runtime has to
/// live on its own std thread — a nested `Runtime::block_on` there would
/// panic ("Cannot start a runtime from within a runtime").
#[derive(Debug)]
struct CuaStager {
    http_base: String,
    host: String,
    token: String,
}

impl remuda_protocol::ToolMediaStager for CuaStager {
    fn max_bytes(&self) -> u64 {
        25 * 1024 * 1024
    }

    fn stage(
        &self,
        name: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<remuda_protocol::Id, remuda_protocol::ToolMediaError> {
        let http_base = self.http_base.clone();
        let host = self.host.clone();
        let token = self.token.clone();
        let name = name.to_owned();
        let media_type = media_type.to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(Err(remuda_protocol::ToolMediaError::Unstageable(format!(
                        "runtime: {error}"
                    ))));
                    return;
                }
            };
            let result = runtime.block_on(async move {
                let response = reqwest::Client::new()
                    .post(format!("{http_base}/v1/hosts/{host}/files/objects",))
                    .bearer_auth(&token)
                    .header("Content-Type", "application/octet-stream")
                    .query(&[("name", name.as_str()), ("mediaType", media_type.as_str())])
                    .body(bytes)
                    .send()
                    .await
                    .map_err(|error| {
                        remuda_protocol::ToolMediaError::Unstageable(format!("upload: {error}"))
                    })?;
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(remuda_protocol::ToolMediaError::Unstageable(format!(
                        "hub refused: {status}"
                    )));
                }
                let staged: Value = serde_json::from_str(&body).map_err(|error| {
                    remuda_protocol::ToolMediaError::Unstageable(format!("json: {error}"))
                })?;
                let object_id =
                    staged
                        .get("objectId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            remuda_protocol::ToolMediaError::Unstageable("no objectId".into())
                        })?;
                remuda_protocol::Id::try_from(object_id.to_owned()).map_err(|error| {
                    remuda_protocol::ToolMediaError::Unstageable(format!("id: {error}"))
                })
            });
            let _ = tx.send(result);
        });
        rx.recv_timeout(std::time::Duration::from_secs(95))
            .map_err(|error| {
                remuda_protocol::ToolMediaError::Unstageable(format!("stager join: {error}"))
            })?
    }
}

/// c-cua-media: append NATIVE claude transcript records — an MCP tool call and
/// a tool result whose content carries a base64 screenshot — and let the REAL
/// remuda-journal mapper fold them. The mapper's stager uploads the PNG to the
/// object route, so what gets journaled is the object reference; the mirrored
/// `toolUseResult` is scrubbed too. Drives the same path a production Node
/// takes, rather than hand-building folded blocks (D-045 §6.2).
#[allow(clippy::too_many_arguments)]
async fn append_cua_scenario(
    ws: &mut NodeWs,
    addr: SocketAddr,
    host: &str,
    token: &str,
    instance_id: &str,
    prompt: &str,
    command_id: Option<&str>,
    mut n: u64,
) -> Result<u64> {
    // The composer's own bubble with the SAME commandId shape the normal
    // composer-send path uses, so the optimistic bubble folds away instead
    // of lingering on 等待发送.
    let node = command_id
        .map(|id| {
            // Strip the prefix and use the bare UUID, exactly like
            // append_command_user: obj_cmd_<uuid> is rejected by typed id
            // consumers.
            id.strip_prefix("cmd_")
                .map_or_else(|| format!("obj_node_{n}"), |uuid| format!("obj_{uuid}"))
        })
        .unwrap_or_else(|| format!("obj_legacy_{n}"));
    n = append_user_message(ws, instance_id, n, prompt, command_id, &node).await?;

    let http_base = format!("http://{addr}");
    let stager = std::sync::Arc::new(CuaStager {
        http_base,
        host: host.to_owned(),
        token: token.to_owned(),
    });
    let instance = remuda_protocol::InstanceId::try_from(instance_id.to_owned())?;
    let image = cua_png_base64();
    // Native records, mirroring what `claude` writes when an MCP tool returns
    // a screenshot: the assistant tool_use, then the user tool_result. The
    // sidecar duplicates the content array — the mapper must scrub both.
    let assistant_line = json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_cua_get_app_state_1",
                "name": "mcp__codex-computer-use__get_app_state",
                "input": { "app": "com.apple.Safari" },
            }],
        },
    });
    let result_line = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_cua_get_app_state_1",
                "content": [
                    { "type": "text", "text": "window state captured" },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": image,
                        },
                    },
                ],
            }],
        },
        // Claude mirrors the content here; the producer must scrub it.
        "toolUseResult": {
            "content": [
                { "type": "text", "text": "window state captured" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": image,
                    },
                },
            ],
        },
    });

    // Fold on a blocking thread: the stager parks until the Hub answers, so
    // this must not run on the fake node's tokio workers (same rule the
    // production driver prefold follows).
    let host_owned = host.to_owned();
    let events = tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
        let mut events = Vec::new();
        let ctx = remuda_journal::MapContext::claude_file(
            instance.clone(),
            remuda_protocol::Id::new("obj")?,
            remuda_protocol::HostId::try_from(host_owned)?,
            "cua-session",
            remuda_protocol::SourceChannel::Transcript,
        )
        .with_media_stager(Some(stager));
        let mut ids = remuda_journal::NativeIds::new(instance.as_id().as_str());
        for record in [assistant_line, result_line] {
            let line = serde_json::to_vec(&record)?;
            let cursor = remuda_protocol::FileCursor {
                file_identity: remuda_protocol::Id::new("obj")?,
                file_generation: remuda_protocol::U64(1),
                length: remuda_protocol::U64(line.len() as u64),
                digest: remuda_journal::digest_of(&line),
                offset: remuda_protocol::U64(line.len() as u64),
            };
            for envelope in remuda_journal::map_claude_line(&ctx, &mut ids, &line, &cursor)? {
                // The observation payload serializes externally tagged as
                // {"kind","payload"}; journal.append wants the kind plus its
                // inner payload (it re-wraps), so pull the payload out.
                let tagged = serde_json::to_value(&envelope.body)?;
                let kind = tagged
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("mapped envelope has no kind: {tagged}"))?
                    .to_owned();
                let payload = tagged
                    .get("payload")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("mapped envelope has no payload: {tagged}"))?;
                events.push(json!({ "kind": kind, "payload": payload }));
            }
        }
        Ok(events)
    })
    .await??;
    for event in events {
        let kind = event
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("mapped event missing kind"))?
            .to_owned();
        let payload = event
            .get("payload")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mapped event missing payload"))?;
        n = append_event(ws, instance_id, n, &kind, payload).await?;
    }

    n = append_journal(
        ws,
        instance_id,
        n,
        "assistant",
        "cua screenshot staged through the object store",
    )
    .await?;
    n = append_native_status(ws, instance_id, n, "idle").await?;
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
            "launchedAt": wf_timestamp(-21_572),
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
            wf_timestamp(-12_000),
            Value::Null,
            wf_timestamp(-2_000),
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
            Value::Null,
            Value::Null,
            Value::Null,
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
    started_at: Value,
    ended_at: Value,
    last_progress_at: Value,
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
        "startedAt": started_at,
        "endedAt": ended_at,
        "lastProgressAt": last_progress_at,
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

/// c-ghostbadge: journal `interaction.requested` carrying the full entity.
/// This is the event that creates the Hub's DURABLE `interactions` row for a
/// hook card. The fake node's normal cards are live-only (served from the
/// in-memory map through `interaction.list`); a card journaled this way is
/// exactly what a hard-killed instance leaves behind in the Hub — the row
/// stays `state='pending'` even after the instance lifecycle settles exited.
async fn append_interaction_requested(
    ws: &mut NodeWs,
    frame_queue: &mut std::collections::VecDeque<String>,
    instance_id: &str,
    n: u64,
    card: &Value,
) -> Result<u64> {
    let seq = n + 1;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": format!("j{seq}"), "method": "journal.append",
            "params": {
                "instanceId": instance_id,
                "event": {
                    "kind": "interaction.requested",
                    "payload": { "interaction": card }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    // Match the ack by id through the shared frame queue: an unconditional
    // ws.next() could swallow a concurrent interaction.list RPC the Hub fans
    // out over this socket.
    wait_frame_ack(ws, frame_queue, &format!("j{seq}")).await?;
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

// ---------------------------------------------------------------------------
// Read-only host files fixture (host-files.hub.spec.ts / evidence).
//
// A deliberately small, real-filesystem implementation of the two Node RPCs:
// it lists actual seeded directories and uploads actual bytes to the Hub with
// the durable host token, so the spec exercises the full Hub -> Node ->
// objects -> Hub loop rather than scripted answers.
// ---------------------------------------------------------------------------

const HOST_FILES_WORKSPACE_ROOT: &str = "/tmp/remuda-e2e";
const HOST_FILES_SECOND_ROOT: &str = "/tmp/remuda-e2e-second";
const HOST_FILES_SCRATCH_DIR: &str = "remuda-hostfiles-e2e";

/// Seed the directories the host-files spec and evidence read.
fn seed_host_file_fixtures() -> Result<()> {
    let root = std::path::Path::new(HOST_FILES_WORKSPACE_ROOT);
    std::fs::create_dir_all(root.join("host-files-dir"))?;
    std::fs::write(root.join("host-files-e2e.txt"), b"remuda host files e2e\n")?;
    std::fs::write(
        root.join("host-files-dir").join("inside.txt"),
        b"nested entry\n",
    )?;
    let second = std::path::Path::new(HOST_FILES_SECOND_ROOT);
    std::fs::create_dir_all(second)?;
    std::fs::write(second.join("other.txt"), b"second workspace\n")?;
    let scratch = std::env::temp_dir().join(HOST_FILES_SCRATCH_DIR);
    std::fs::create_dir_all(&scratch)?;
    std::fs::write(scratch.join("scratch.txt"), b"scratch area\n")?;
    Ok(())
}

/// Resolve the workspace/relPath selector the same way the real Node does:
/// lexical `..`/absolute refusal, then canonical containment under the root
/// (or a `remuda-*` first component under /tmp).
fn host_files_resolve(workspace_id: &str, rel_path: &str) -> Result<std::path::PathBuf> {
    let (anchor, scratch) = match workspace_id {
        "wsp_e2e" => (std::fs::canonicalize(HOST_FILES_WORKSPACE_ROOT)?, false),
        "wsp_e2e_second" => (std::fs::canonicalize(HOST_FILES_SECOND_ROOT)?, false),
        "tmp" => (std::fs::canonicalize(std::env::temp_dir())?, true),
        other => {
            return Err(anyhow!("workspace {other} is not registered on this Node"));
        }
    };
    let rel_path = rel_path.trim();
    let mut joined = anchor.clone();
    if !rel_path.is_empty() {
        let rel = std::path::Path::new(rel_path);
        if rel.is_absolute() {
            return Err(anyhow!(
                "host file path must be relative to the workspace root"
            ));
        }
        for component in rel.components() {
            use std::path::Component;
            match component {
                Component::Normal(segment) => joined.push(segment),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(anyhow!(
                        "host file path {rel_path} escapes the workspace: '..' is not allowed"
                    ));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(anyhow!(
                        "host file path must be relative to the workspace root"
                    ));
                }
            }
        }
    }
    let canonical = std::fs::canonicalize(&joined)?;
    if !canonical.starts_with(&anchor) {
        return Err(anyhow!(
            "host file path {rel_path} escapes the workspace root"
        ));
    }
    if scratch
        && !canonical
            .strip_prefix(&anchor)?
            .components()
            .find_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("remuda-"))
    {
        return Err(anyhow!(
            "host file path {rel_path} is outside the remuda-* scratch area"
        ));
    }
    Ok(canonical)
}

async fn host_files_answer(
    method: &str,
    params: &Value,
    hub: SocketAddr,
    host: &str,
    token: &str,
) -> Result<Value> {
    let workspace_id = params
        .get("workspaceId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let rel_path = params.get("relPath").and_then(Value::as_str).unwrap_or("");
    let target = host_files_resolve(workspace_id, rel_path)?;
    if method == "host.files.list" {
        let metadata = std::fs::symlink_metadata(&target)?;
        if !metadata.is_dir() {
            return Err(anyhow!("{} is not a directory", target.display()));
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&target)? {
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::MetadataExt;
                meta.mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0u32;
            let mtime = meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": kind,
                "size": meta.len(),
                "mtime": mtime,
                "mode": mode,
            }));
        }
        entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        return Ok(json!({
            "workspaceId": workspace_id,
            "path": target.display().to_string(),
            "entries": entries,
        }));
    }
    // host.files.read: regular files only, then a real upload to the Hub.
    let metadata = std::fs::symlink_metadata(&target)?;
    if !metadata.file_type().is_file() {
        return Err(anyhow!("{} is not a regular file", target.display()));
    }
    let bytes = std::fs::read(&target)?;
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("host-file");
    let response = reqwest::Client::new()
        .post(format!("http://{hub}/v1/hosts/{host}/files/objects"))
        .bearer_auth(token)
        .header("Content-Type", "application/octet-stream")
        .query(&[("name", name)])
        .body(bytes)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "Hub refused host file staging with {status}: {text}"
        ));
    }
    Ok(serde_json::from_str::<Value>(&text)?)
}
