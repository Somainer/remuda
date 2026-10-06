//! c-dirpicker route tests:
//! - `GET /v1/hosts/{id}/dirs` proxies `host.dirs.list` for Human operators
//!   only (Bot and Agent are 403; missing host 404; offline host 409);
//! - `DELETE /v1/hosts/{id}/workspaces` is refused with 409 while a live
//!   session uses the workspace, and the refusal reaches the Node never;
//!   with no users the two-phase unregister settles normally.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, NodeTransport, TransportKind, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const WORKSPACE: &str = "wsp_dirpicker";
const ROOT: &str = "/srv/remuda-e2e";

/// Scripted Node: answers the directory browser and the two-phase workspace
/// mutation, recording every method the Hub forwarded.
struct FakeNode {
    calls: Mutex<Vec<String>>,
}

impl FakeNode {
    fn recorded(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl NodeTransport for FakeNode {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, remuda_hub::HubError>> + Send + '_>>
    {
        let method = method.to_owned();
        Box::pin(async move {
            self.calls.lock().unwrap().push(method.clone());
            let reply = match method.as_str() {
                "host.dirs.list" => {
                    // Simulate the real Node's authoritative resolution:
                    //  * an alias directory ("/srv/...") resolves to the
                    //    registered ROOT;
                    //  * a real symlink alias ("link-to-e2e") is REFUSED
                    //    (the Node's no-follow walk never follows it);
                    //  * a name with a trailing space is a real directory and
                    //    resolves verbatim to the registered root.
                    let requested = params.get("path").and_then(Value::as_str).unwrap_or("");
                    if requested.contains("link-to-e2e") {
                        json!({"error": {"code": -32602, "message":
                            "the browsed path is outside the directories this Node allows workspaces in, \
                             or is not an accessible directory"}})
                    } else {
                        let canonical = if requested.is_empty() {
                            "/home/remuda"
                        } else {
                            requested.trim_end()
                        };
                        json!({
                            "path": canonical,
                            "parent": null,
                            "home": "/home/remuda",
                            "roots": ["/home/remuda", canonical],
                            "workspaces": [],
                            "dirs": [{ "name": "projects" }],
                            "truncated": false,
                        })
                    }
                }
                "workspace.list" => json!({
                    "workspaceRevision": 1,
                    "workspaces": [{ "workspaceId": WORKSPACE, "root": ROOT }],
                }),
                "workspace.register" | "workspace.unregister" => {
                    let command_id = params["commandId"].clone();
                    let phase = params["phase"].as_str().unwrap();
                    json!({
                        "workspaceRevision": if phase == "commit" { 2 } else { 1 },
                        "workspaces": [],
                        "workspaceId": WORKSPACE,
                        "commandId": command_id,
                        "phase": if phase == "prepare" { "prepared" } else { "settled" },
                    })
                }
                other => {
                    json!({"error": {"code": -32601, "message": format!("unexpected {other}")}})
                }
            };
            Ok(Some(reply))
        })
    }

    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, remuda_hub::HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(true)))
    }
}

async fn json_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(text) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            text.len()
        ));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(text) = body {
        request.extend_from_slice(text.as_bytes());
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("header terminator")?;
    let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Ok((
        status,
        String::from_utf8_lossy(&buf[split + 4..]).to_string(),
    ))
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

async fn login(addr: SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({"bootstrapToken": bootstrap, "deviceName": "dirpicker-phone"}).to_string();
    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!(
                "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("header terminator")?;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    assert!(head.contains(" 200 "), "{head}");
    cookie_from(&head).context("set-cookie")
}

struct Fixture {
    hub: remuda_hub::RunningHub,
    cookie: String,
    node: Arc<FakeNode>,
    host: String,
}

async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host = HostId::new().as_id().as_str().to_owned();
    hub.test_insert_host(&host).await?;
    let node = Arc::new(FakeNode {
        calls: Mutex::new(Vec::new()),
    });
    hub.test_set_node_transport(&host, node.clone()).await;
    // Keep the temp dir alive for the fixture's life.
    std::mem::forget(dir);
    Ok(Fixture {
        hub,
        cookie,
        node,
        host,
    })
}

#[tokio::test]
async fn human_operator_browses_directories() -> Result<()> {
    let fixture = fixture().await?;
    let (status, body) = json_request(
        fixture.hub.addr,
        "GET",
        &format!(
            "/v1/hosts/{}/dirs?path=/home/remuda&showHidden=true",
            fixture.host
        ),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let listing = serde_json::from_str::<Value>(body.trim())?;
    assert_eq!(listing["path"], json!("/home/remuda"));
    assert_eq!(listing["dirs"][0]["name"], json!("projects"));

    // Empty selector is allowed too (the Node picks its default start).
    let (status, _) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/dirs", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn agent_and_bot_origins_are_403() -> Result<()> {
    let fixture = fixture().await?;
    let agent = fixture
        .hub
        .test_mint_agent_token("dirpicker-agent", "ins_unrelated")
        .await?;
    let (status, _) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/dirs", fixture.host),
        &[("Authorization", &format!("Bearer {agent}"))],
        None,
    )
    .await?;
    assert_eq!(
        status, 403,
        "an agent must not browse unregistered directories"
    );

    let bot = fixture.hub.mint_bot_device_token("dirpicker-bot").await?;
    let (status, _) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/dirs", fixture.host),
        &[("Authorization", &format!("Bearer {bot}"))],
        None,
    )
    .await?;
    assert_eq!(
        status, 403,
        "a bot operator may use file views but not browse"
    );
    // The refusal happens at the Hub: the Node saw no browsing request.
    assert!(
        !fixture
            .node
            .recorded()
            .iter()
            .any(|method| method == "host.dirs.list"),
        "{:?}",
        fixture.node.recorded()
    );
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_and_offline_hosts_are_rejected() -> Result<()> {
    let fixture = fixture().await?;
    let ghost = HostId::new().as_id().as_str().to_owned();
    let (status, _) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{ghost}/dirs"),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 404);
    fixture.hub.test_disconnect_node(&fixture.host).await;
    let (status, _) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/dirs", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 409);
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unregister_is_refused_while_a_session_is_live_and_settles_after_it_ends() -> Result<()> {
    let fixture = fixture().await?;
    let store = fixture.hub.store().expect("store open");

    // Observe the workspace snapshot exactly like the page's GET would.
    let (status, body) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // A live session in the workspace blocks the unregister at the Hub.
    let instance = store
        .insert_instance(
            fixture.host.clone(),
            Some(WORKSPACE.to_owned()),
            "assistant".into(),
            "claude-pty".into(),
            Some("live session".into()),
            json!({}),
        )
        .await?;
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": ROOT}).to_string()),
    )
    .await?;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("live session"), "{body}");
    assert!(
        !fixture
            .node
            .recorded()
            .iter()
            .any(|method| method == "workspace.unregister"),
        "the refusal must never reach the Node: {:?}",
        fixture.node.recorded()
    );

    // A lexical canonical alias for the same busy root must not slip past the
    // occupancy guard (exact-string lookup would miss it).
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": "/srv/./remuda-e2e/../remuda-e2e/"}).to_string()),
    )
    .await?;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("live session"), "{body}");
    assert!(
        !fixture
            .node
            .recorded()
            .iter()
            .any(|method| method == "workspace.unregister"),
        "the aliased refusal must never reach the Node: {:?}",
        fixture.node.recorded()
    );

    // Ended sessions keep their history row but no longer block removal.
    fixture
        .hub
        .test_mark_instance_exited(&instance.instance_id)
        .await?;
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": ROOT}).to_string()),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("workspaceRevision"));
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn node_prepare_occupancy_refusal_surfaces_as_409_not_400() -> Result<()> {
    // The Hub store has no session rows for this workspace, so its own guard
    // passes and the command is queued; the Node then refuses the prepare.
    // That authoritative refusal must map to 409 with its reason visible.
    let fixture = fixture().await?;
    let observe = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(observe.0, 200);
    fixture
        .hub
        .test_set_node_reply(
            &fixture.host,
            Some(json!({
                "error": {
                    "code": -32602,
                    "message": format!(
                        "workspace {ROOT} is still used by 3 live session(s); \
                         end them before removing the directory (session history is kept)"
                    )
                }
            })),
        )
        .await;
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": ROOT}).to_string()),
    )
    .await?;
    assert_eq!(status, 409, "node occupancy refusal must be 409: {body}");
    assert!(body.contains("3 live session(s)"), "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_failed_hub_row_does_not_override_a_live_node_session() -> Result<()> {
    // Round 3 item 5: real liveness is the Node's job. A Hub row marked
    // `failed` (a legacy row written before the process-end semantics, or a
    // stale projection) must pass the Hub's own ended check, but a live
    // session on the Node still makes the Node refuse the prepare — which
    // surfaces as 409 with the visible reason.
    let fixture = fixture().await?;
    let _ = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    // Hub-side row: failed = ended for the occupancy query, so the DELETE
    // proceeds past the Hub guard and reaches the Node.
    let store = fixture.hub.store().expect("store open");
    let instance = store
        .insert_instance(
            fixture.host.clone(),
            Some(WORKSPACE.to_owned()),
            "assistant".into(),
            "claude-pty".into(),
            Some("a legacy failed row whose process the Node still runs".into()),
            json!({}),
        )
        .await?;
    fixture
        .hub
        .test_mark_instance_failed(&instance.instance_id)
        .await?;
    fixture
        .hub
        .test_set_node_reply(
            &fixture.host,
            Some(json!({
                "error": {
                    "code": -32602,
                    "message": format!(
                        "workspace {ROOT} is still used by 1 live session(s); \
                         end them before removing the directory (session history is kept)"
                    )
                }
            })),
        )
        .await;
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": ROOT}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 409,
        "live Node session must win over a failed-looking Hub row: {body}"
    );
    assert!(body.contains("1 live session(s)"), "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn symlink_alias_and_real_trailing_space_hit_the_task_guard_and_fail_closed() -> Result<()> {
    // Round 4 item 3: no instance rows, only an active task binding.
    //  * a real symlink alias cannot be resolved to the registered root by
    //    the Node (no-follow browse refuses), so the DELETE fails closed with
    //    400 — it must NOT skip the task guard and proceed to unregister;
    //  * a registered directory whose name REALLY ends in a space is sent
    //    verbatim; the Node's browse resolves it (trim_end in the fake, but
    //    the Hub never trims), so the active task blocks it with 409.
    let fixture = fixture().await?;
    let _ = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    fixture
        .hub
        .test_insert_bound_task("tsk_dirbind4", "prj_a", &fixture.host, WORKSPACE, "running")
        .await?;

    // (1) Symlink alias: resolution fails → fail closed 400, no unregister.
    let calls_before = fixture.node.recorded().len();
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": "/somewhere/link-to-e2e"}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 400,
        "a symlink the Node refuses to resolve must fail closed: {body}"
    );
    let unregister_calls = fixture
        .node
        .recorded()
        .iter()
        .filter(|method| method.as_str() == "workspace.unregister")
        .count();
    assert_eq!(
        unregister_calls, 0,
        "no unregister after a failed resolution"
    );
    let _ = calls_before;

    // (2) Real trailing-space name, sent verbatim: Node resolves it to ROOT,
    //     task guard blocks with 409 before any unregister command.
    let path_with_space = format!("{ROOT} ");
    let before = fixture
        .node
        .recorded()
        .iter()
        .filter(|m| m.as_str() == "workspace.unregister")
        .count();
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": path_with_space}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 409,
        "trailing-space registered root must be task-blocked: {body}"
    );
    assert!(body.contains("active task(s)"), "{body}");
    let after = fixture
        .node
        .recorded()
        .iter()
        .filter(|m| m.as_str() == "workspace.unregister")
        .count();
    assert_eq!(
        before, after,
        "no unregister may be sent for the blocked path"
    );

    fixture.hub.shutdown().await;
    Ok(())
}

mod real_node {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    async fn raw_http(
        addr: SocketAddr,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<&str>,
    ) -> Result<(u16, String)> {
        let mut stream = TcpStream::connect(addr).await?;
        let content_length = body.map(str::len).unwrap_or(0);
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {content_length}\r\n"
        );
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
        }
        if let Some(cookie) = cookie {
            head.push_str(&format!("Cookie: {cookie}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;
        if let Some(body) = body {
            stream.write_all(body.as_bytes()).await?;
        }
        let mut buf = Vec::new();
        AsyncReadExt::read_to_end(&mut stream, &mut buf).await?;
        let split = buf
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .context("header terminator")?;
        let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
        let status = head_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        Ok((
            status,
            String::from_utf8_lossy(&buf[split + 4..]).to_string(),
        ))
    }

    async fn login(addr: SocketAddr, bootstrap: &str) -> Result<String> {
        let body = json!({"bootstrapToken": bootstrap, "deviceName": "realnode-phone"}).to_string();
        let mut stream = TcpStream::connect(addr).await?;
        stream
            .write_all(
                format!(
                    "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                     Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
        let mut buf = Vec::new();
        AsyncReadExt::read_to_end(&mut stream, &mut buf).await?;
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("headers")?;
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        assert!(head.contains(" 200 "), "{head}");
        super::cookie_from(&head).context("set-cookie")
    }

    async fn enroll_token(addr: SocketAddr, cookie: &str) -> Result<String> {
        let (status, rest) = raw_http(
            addr,
            "POST",
            "/v1/hosts/enroll-token",
            Some(cookie),
            Some("{}"),
        )
        .await?;
        anyhow::ensure!(status == 200, "enroll-token {status}");
        let value: Value = serde_json::from_str(rest.trim())?;
        value["token"]
            .as_str()
            .map(str::to_owned)
            .context("enroll token")
    }

    /// Scripted node over a REAL websocket: announces a live workspace and
    /// refuses unregister prepare while `live` is set (a real child process
    /// the test owns keeps that claim honest).
    async fn serve_live_node(
        addr: SocketAddr,
        enroll: String,
        host_id: String,
        live: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
                "params": {
                    "hostId": host_id, "nodeVersion": "0.1.0-realnode",
                    "host": {
                        "workspaceRevision": 1,
                        "workspaces": [{ "workspaceId": WORKSPACE, "hostId": host_id, "root": ROOT }],
                        "herdr": { "version": "0.9.0" }
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
        // Wait for the hello result.
        loop {
            let Some(Ok(Message::Text(text))) = ws.next().await else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)?;
            if value.get("id").and_then(Value::as_str) == Some("hello") {
                anyhow::ensure!(value.get("result").is_some(), "hello rejected: {text}");
                break;
            }
        }
        while live.load(std::sync::atomic::Ordering::Relaxed) {
            let frame = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
            let Ok(Some(Ok(Message::Text(text)))) = frame else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            // JSON-RPC errors travel at the frame top level (the Hub's
            // call_node reads response.error there).
            let frame = match method {
                "workspace.unregister" => json!({
                    "error": {
                        "code": -32602,
                        "message": format!(
                            "workspace {ROOT} is still used by 1 live session(s); \
                             end them before removing the directory (session history is kept)"
                        )
                    }
                }),
                "workspace.list" => json!({
                    "result": {
                        "workspaceRevision": 1,
                        "workspaces": [{ "workspaceId": WORKSPACE, "hostId": host_id, "root": ROOT }]
                    }
                }),
                "host.dirs.list" => json!({
                    "result": {
                        "path": ROOT, "parent": null, "home": ROOT,
                        "roots": [ROOT], "workspaces": [], "dirs": [], "truncated": false
                    }
                }),
                other => json!({
                    "error": {"code": -32601, "message": format!("unexpected {other}")}
                }),
            };
            let response = if let Some(error) = frame.get("error") {
                json!({"jsonrpc": "2.0", "id": id, "error": error})
            } else {
                json!({"jsonrpc": "2.0", "id": id, "result": frame.get("result")})
            };
            ws.send(Message::Text(response.to_string().into())).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_real_attached_node_refuses_unregister_while_its_process_is_alive() -> Result<()> {
        // Round 4 item 9: a REAL websocket node holds an actual child process.
        // The Hub row says failed (stale), but the Node's prepare refuses due
        // to the live process; the Hub surfaces 409.
        let dir = tempfile::tempdir()?;
        let config = HubConfig::for_test(dir.path().join("data"));
        let bootstrap = config.bootstrap_token.clone();
        let hub = remuda_hub::spawn(config).await?;
        let addr = hub.addr;
        let cookie = login(addr, &bootstrap).await?;
        let enroll = enroll_token(addr, &cookie).await?;
        let host = HostId::new();
        let host_id = host.as_id().to_string();

        // The real process the node "runs" for the duration of the test.
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "trap 'exit 0' TERM; while :; do sleep 0.2; done"])
            .spawn()?;
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let node = {
            let live = live.clone();
            let enroll = enroll.clone();
            let host_id = host_id.clone();
            tokio::spawn(async move {
                serve_live_node(addr, enroll, host_id, live)
                    .await
                    .map_err(|e| tracing::warn!("live node ended: {e}"))
                    .ok();
            })
        };

        // Wait for the node to come online through the Hub's own GET.
        for _ in 0..50 {
            let (status, body) = raw_http(
                addr,
                "GET",
                &format!("/v1/hosts/{host_id}"),
                Some(&cookie),
                None,
            )
            .await?;
            if status == 200 && body.contains("\"online\":true") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Only the Hub row says failed.
        let instance = hub
            .store()
            .expect("store")
            .insert_instance(
                host_id.clone(),
                Some(WORKSPACE.to_owned()),
                "assistant".into(),
                "shell-pty".into(),
                Some("stale failed row; real node process is alive".to_owned()),
                json!({}),
            )
            .await?;
        hub.test_mark_instance_failed(&instance.instance_id).await?;

        let (status, body) = raw_http(
            addr,
            "DELETE",
            &format!("/v1/hosts/{host_id}/workspaces"),
            Some(&cookie),
            Some(&json!({"path": ROOT}).to_string()),
        )
        .await?;
        assert_eq!(status, 409, "real live node must refuse unregister: {body}");
        assert!(body.contains("1 live session(s)"), "{body}");

        // Tear down the real process and node task.
        child.kill().await?;
        let _ = child.wait().await;
        live.store(false, std::sync::atomic::Ordering::Relaxed);
        node.await?;
        Ok(())
    }
}

// ── Round 4 item 5: invalid binding ids reject before a task row exists ────

mod invalid_binding {
    use super::*;

    #[tokio::test]
    async fn malformed_binding_ids_return_400_and_leave_no_task_row() -> Result<()> {
        // Binding ids/mode are parsed BEFORE the task row is created, so a
        // 400 leaves nothing behind. Task count is read over HTTP.
        let fixture = fixture().await?;

        // Create a project the cookie's device can dispatch into.
        let (status, project_body) = json_request(
            fixture.hub.addr,
            "POST",
            "/v1/projects",
            &[("Cookie", &fixture.cookie)],
            Some(&json!({"name": "r4-item5"}).to_string()),
        )
        .await?;
        assert_eq!(status, 200, "project create: {project_body}");
        let project_id: String = serde_json::from_str::<Value>(&project_body)?
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("project id")?;

        async fn task_count(fixture: &Fixture, project_id: &str) -> Result<usize> {
            let (_, body) = json_request(
                fixture.hub.addr,
                "GET",
                &format!("/v1/tasks?project={project_id}"),
                &[("Cookie", &fixture.cookie)],
                None,
            )
            .await?;
            let value: Value = serde_json::from_str(&body)?;
            Ok(value
                .get("items")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len)
                .unwrap_or(0))
        }
        let before = task_count(&fixture, &project_id).await?;

        let cases = [
            json!({
                "projectId": project_id, "title": "t1", "intent": "i1",
                "workspaceBinding": {"mode": "reuse", "hostId": "not-a-host", "workspaceId": WORKSPACE}
            }),
            json!({
                "projectId": project_id, "title": "t2", "intent": "i2",
                "workspaceBinding": {"mode": "reuse", "hostId": fixture.host, "workspaceId": "not-a-wsp"}
            }),
            json!({
                "projectId": project_id, "title": "t3", "intent": "i3",
                "workspaceBinding": {"mode": "sideways", "hostId": fixture.host, "workspaceId": WORKSPACE}
            }),
        ];
        for body in cases {
            let (status, text) = json_request(
                fixture.hub.addr,
                "POST",
                "/v1/tasks",
                &[("Cookie", &fixture.cookie)],
                Some(&body.to_string()),
            )
            .await?;
            assert_eq!(status, 400, "expected 400 for {body}, got {status}: {text}");
        }

        let after = task_count(&fixture, &project_id).await?;
        assert_eq!(
            before, after,
            "a rejected binding must never leave a task row ({before} -> {after})"
        );
        fixture.hub.shutdown().await;
        Ok(())
    }
}
