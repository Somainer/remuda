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
                "host.dirs.list" => json!({
                    "path": params.get("path").and_then(Value::as_str).unwrap_or("/home/remuda"),
                    "parent": null,
                    "home": "/home/remuda",
                    "roots": ["/home/remuda", "/srv"],
                    "workspaces": [],
                    "dirs": [{ "name": "projects" }, { "name": "tools" }],
                    "truncated": false,
                }),
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
