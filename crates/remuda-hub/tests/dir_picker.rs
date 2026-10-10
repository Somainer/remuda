//! c-dirpicker route tests:
//! - `GET /v1/hosts/{id}/dirs` proxies `host.dirs.list` for Human operators
//!   only (Bot and Agent are 403; missing host 404; offline host 409);
//! - `DELETE /v1/hosts/{id}/workspaces` is refused with 409 while a live
//!   session uses the workspace, and the refusal reaches the Node never;
//!   with no users the two-phase unregister settles normally.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
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
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

// Real branded-id spellings (project member validation parses them).
const WORKSPACE: &str = "wsp_01993ab0-0000-7000-8000-0000000000a1";
const WORKSPACE2: &str = "wsp_01993ab0-0000-7000-8000-0000000000a2";
const ROOT: &str = "/srv/remuda-e2e";

/// Scripted Node: answers the directory browser, the read-only
/// `workspace.resolve` RPC and the two-phase workspace mutation, recording
/// every method the Hub forwarded.
///
/// Round 6 item 1: identity for a removal comes from `workspace.resolve`,
/// which resolves with unregister's REALPATH semantics. `host.dirs.list`
/// stays a BROWSE view: it collapses `..` LEXICALLY before its no-follow
/// walk, so `/allowed/link/../proj` names a different real directory than
/// resolve does. This scripted node emulates both production outcomes on a
/// REAL temp filesystem (actual symlink, actual trailing-space directory).
struct FakeNode {
    calls: Mutex<Vec<String>>,
    /// The real registered roots, canonicalized at fixture setup:
    /// `root ` (a name ending in a space) and `other/proj`.
    real_roots: std::sync::Mutex<Vec<std::path::PathBuf>>,
    /// r7 item 1: the next N `commit` phases of workspace.unregister fail
    /// with a Node-style conflict (as if occupancy raced in); the Hub must
    /// then send the abort phase.
    fail_unregister_commits: std::sync::atomic::AtomicU32,
    /// r8 item 2 race B: the next `abort` reports the commit had already
    /// SETTLED on the Node, returning phase "settled" with the post-commit
    /// snapshot (the target workspace removed, revision advanced).
    abort_reports_settled: std::sync::atomic::AtomicBool,
    /// Every abort frame the Hub sent, in order.
    aborts: Mutex<Vec<Value>>,
}

impl FakeNode {
    fn recorded(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn real_roots(&self) -> Vec<std::path::PathBuf> {
        self.real_roots.lock().unwrap().clone()
    }

    fn aborts(&self) -> Vec<Value> {
        self.aborts.lock().unwrap().clone()
    }

    fn fail_next_unregister_commit(&self) {
        self.fail_unregister_commits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Make the next `abort` frame answer as if commit had already settled on
    /// the Node (r8 item 2 race B).
    fn next_abort_reports_settled(&self) {
        self.abort_reports_settled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Lexical `..` collapse, mirroring the production directory browser's
/// pre-walk normalization (dir_browser.rs `lexical_normalize`): no filesystem
/// access, clamped at `/`.
fn lexical_normalize(raw: &str) -> String {
    if !raw.starts_with('/') {
        return raw.to_owned();
    }
    let mut stack: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            name => stack.push(name),
        }
    }
    format!("/{}", stack.join("/"))
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
                    // The BROWSE view. Empty selector starts at the first
                    // registered root; any other selector is normalized
                    // LEXICALLY (collapsing `..` without touching the disk)
                    // and then canonicalized, exactly like the production
                    // no-follow walk does. A lexical `..` therefore lands on
                    // a different real directory than `workspace.resolve`.
                    let requested = params.get("path").and_then(Value::as_str).unwrap_or("");
                    let roots = self.real_roots();
                    let first = roots
                        .first()
                        .cloned()
                        .unwrap_or_else(|| std::path::PathBuf::from(ROOT));
                    if requested.is_empty() {
                        json!({
                            "path": first.display().to_string(),
                            "parent": null,
                            "home": first.display().to_string(),
                            "roots": roots.iter().map(|p| json!(p.display().to_string())).collect::<Vec<_>>(),
                            "workspaces": [],
                            "dirs": [{ "name": "projects" }],
                            "truncated": false
                        })
                    } else {
                        let lexical = lexical_normalize(requested);
                        match std::fs::canonicalize(&lexical) {
                            Ok(canonical) => json!({
                                "path": canonical.display().to_string(),
                                "parent": null,
                                "home": first.display().to_string(),
                                "roots": roots.iter().map(|p| json!(p.display().to_string())).collect::<Vec<_>>(),
                                "workspaces": [],
                                "dirs": [{ "name": "projects" }],
                                "truncated": false
                            }),
                            Err(_) => json!({"error": {"code": -32602, "message":
                                "the browsed path is outside the directories this Node allows workspaces in, \
                                 or is not an accessible directory"}}),
                        }
                    }
                }
                "workspace.resolve" => {
                    // The REMOVAL identity view: REALPATH semantics (the same
                    // canonicalize the production unregister prepare uses),
                    // matched against the registered roots. A lexical-`..`
                    // alias through a symlink resolves to the symlink target's
                    // real sibling, which is the point of round 6 item 1.
                    let requested = params.get("path").and_then(Value::as_str).unwrap_or("");
                    // Prefer the REAL realpath (symlink + `..` semantics); fall
                    // back to the component-normalized spelling so pure
                    // separators/dot aliases (`/root//`, `/root/.`) that
                    // std::fs::canonicalize rejects here still resolve to the
                    // registered row, exactly as the production Node's
                    // realpath does for them (r7 item 2).
                    let normalized = lexical_normalize(requested);
                    let resolved = std::fs::canonicalize(requested)
                        .ok()
                        .or_else(|| std::fs::canonicalize(&normalized).ok())
                        .and_then(|canonical| {
                            self.real_roots()
                                .iter()
                                .enumerate()
                                .find(|(_, root)| *root == &canonical)
                                .map(|(index, root)| (index, root.clone()))
                        });
                    match resolved {
                        Some((0, root)) => json!({
                            "workspaceId": WORKSPACE,
                            "canonicalRoot": root.display().to_string(),
                        }),
                        Some((_, root)) => json!({
                            "workspaceId": WORKSPACE2,
                            "canonicalRoot": root.display().to_string(),
                        }),
                        None => json!({"error": {"code": -32602, "message":
                            format!("workspace {requested} is not registered")}}),
                    }
                }
                "workspace.list" => {
                    // Report the REAL canonical roots (which may be
                    // /private/tmp/… on macOS).
                    let roots = self.real_roots();
                    let workspaces = roots
                        .iter()
                        .enumerate()
                        .map(|(index, root)| {
                            json!({
                                "workspaceId": if index == 0 { WORKSPACE } else { WORKSPACE2 },
                                "hostId": "hst_dirpicker",
                                "root": root.display().to_string(),
                            })
                        })
                        .collect::<Vec<_>>();
                    json!({
                        "workspaceRevision": 1,
                        "workspaces": workspaces,
                    })
                }
                "workspace.register" | "workspace.unregister" => {
                    let command_id = params["commandId"].clone();
                    let phase = params["phase"].as_str().unwrap();
                    let workspace_id = params
                        .get("workspaceId")
                        .and_then(Value::as_str)
                        .unwrap_or(WORKSPACE);
                    // r7 item 1: scripted commit refusal; the Hub must follow
                    // it with an abort, and a later DELETE must still settle.
                    if method == "workspace.unregister"
                        && phase == "commit"
                        && self
                            .fail_unregister_commits
                            .fetch_update(
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                                |n| if n > 0 { Some(n - 1) } else { None },
                            )
                            .is_ok()
                    {
                        json!({"error": {"code": -32000, "message":
                            "workspace /srv/remuda-e2e gained occupancy after unregister prepare; \
                             the removal did not settle"}})
                    } else if method == "workspace.unregister" && phase == "abort" {
                        self.aborts.lock().unwrap().push(params.clone());
                        // r8 item 2 race B: commit had already completed before
                        // the abort arrived — answer "settled" with the
                        // post-commit snapshot (the removed root is gone and
                        // the revision advanced).
                        let settled_on_node = self
                            .abort_reports_settled
                            .swap(false, std::sync::atomic::Ordering::SeqCst);
                        if settled_on_node {
                            let remaining = self
                                .real_roots()
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| *index != 0)
                                .map(|(index, root)| {
                                    json!({
                                        "workspaceId": if index == 0 { WORKSPACE } else { WORKSPACE2 },
                                        "hostId": "hst_dirpicker",
                                        "root": root.display().to_string(),
                                    })
                                })
                                .collect::<Vec<_>>();
                            json!({
                                "workspaceRevision": 2,
                                "workspaces": remaining,
                                "workspaceId": workspace_id,
                                "commandId": command_id,
                                "phase": "settled",
                            })
                        } else {
                            json!({
                                "workspaceRevision": 1,
                                "workspaces": self.real_roots().iter().enumerate()
                                    .map(|(index, root)| json!({
                                        "workspaceId": if index == 0 { WORKSPACE } else { WORKSPACE2 },
                                        "hostId": "hst_dirpicker",
                                        "root": root.display().to_string(),
                                    }))
                                    .collect::<Vec<_>>(),
                                "workspaceId": workspace_id,
                                "commandId": command_id,
                                "phase": "aborted",
                            })
                        }
                    } else {
                        json!({
                            "workspaceRevision": if phase == "commit" { 2 } else { 1 },
                            "workspaces": [],
                            "workspaceId": workspace_id,
                            "commandId": command_id,
                            "phase": if phase == "commit" { "settled" } else if phase == "abort" { "aborted" } else { "prepared" },
                        })
                    }
                }
                "worktree.lease" => json!({
                    "mode": "reuse",
                    "dirKey": ".",
                    "name": ".",
                }),
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

/// Percent-encode a query path (the real fixture root's name ends in a space).
fn urlencoding(path: &str) -> String {
    path.replace(' ', "%20")
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
    /// Registered root whose name ends in a real trailing space.
    real_root: std::path::PathBuf,
    /// A real symlink to `real_root`.
    symlink_path: std::path::PathBuf,
    /// The codex/grok tree: a second registered root reached only by
    /// realpath through a symlink plus `..`.
    alias_other: std::path::PathBuf,
    /// `allowed/link/../proj`: lexical `..` lands on `allowed/proj`,
    /// realpath lands on `other/proj`.
    alias_dotdot: std::path::PathBuf,
    /// Owned so the real temp tree is deleted after the hub shuts down
    /// (c-dirpicker r7 item 9; previously `mem::forget` leaked ~12 trees per
    /// run under the shared $TMPDIR).
    _dir: tempfile::TempDir,
}

async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host = HostId::new().as_id().as_str().to_owned();
    hub.test_insert_host(&host).await?;

    // (1) A REAL directory whose NAME ends in a space (a legal filename byte
    //     the Hub must not trim), plus an actual symlink to it.
    let real_root = dir.path().join("root ");
    std::fs::create_dir_all(&real_root)?;
    let symlink_path = dir.path().join("link-to-e2e");
    std::os::unix::fs::symlink(&real_root, &symlink_path)?;
    let real_root_canonical = std::fs::canonicalize(&real_root)?;

    // (2) The codex/grok two-real-dirs tree (round 6 item 1):
    //
    //     allowed/proj       — real dir A (what a LEXICAL `..` collapse sees)
    //     other/proj         — real dir B, the REGISTERED workspace
    //     allowed/link       — real symlink → other/proj (B itself)
    //
    //     `allowed/link/../proj` realpaths to `/other/proj` (unregister's
    //     semantics: resolve the symlink, then apply `..`) but lexically
    //     normalizes to `/allowed/proj` (the browse view collapses `..` with
    //     no filesystem access). Pointing the link at B (not at `other`) is
    //     what makes the `..` land back inside B.
    let alias_other = dir.path().join("other");
    let alias_root = alias_other.join("proj");
    let alias_allowed = dir.path().join("allowed");
    std::fs::create_dir_all(&alias_root)?;
    std::fs::create_dir_all(alias_allowed.join("proj"))?;
    let alias_link = alias_allowed.join("link");
    std::os::unix::fs::symlink(&alias_root, &alias_link)?;
    let alias_dotdot = alias_allowed.join("link/../proj");
    let alias_root = std::fs::canonicalize(alias_root)?;

    let node = Arc::new(FakeNode {
        calls: Mutex::new(Vec::new()),
        real_roots: std::sync::Mutex::new(vec![real_root_canonical.clone(), alias_root.clone()]),
        fail_unregister_commits: std::sync::atomic::AtomicU32::new(0),
        abort_reports_settled: std::sync::atomic::AtomicBool::new(false),
        aborts: Mutex::new(Vec::new()),
    });
    hub.test_set_node_transport(&host, node.clone()).await;
    Ok(Fixture {
        hub,
        cookie,
        node,
        host,
        real_root: real_root_canonical,
        symlink_path,
        alias_other,
        alias_dotdot,
        _dir: dir,
    })
}

#[tokio::test]
async fn human_operator_browses_directories() -> Result<()> {
    let fixture = fixture().await?;
    let (status, body) = json_request(
        fixture.hub.addr,
        "GET",
        &format!(
            "/v1/hosts/{}/dirs?path={}&showHidden=true",
            fixture.host,
            urlencoding(&fixture.real_root.display().to_string())
        ),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let listing = serde_json::from_str::<Value>(body.trim())?;
    assert_eq!(
        listing["path"],
        json!(fixture.real_root.display().to_string())
    );
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
        Some(&json!({"path": fixture.real_root.display().to_string()}).to_string()),
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

    // DELETE the exact registered (canonical, trailing-space) root: the live
    // session occupancy guard refuses before any unregister.
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": fixture.real_root.display().to_string()}).to_string()),
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
        Some(&json!({"path": fixture.real_root.display().to_string()}).to_string()),
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
        Some(&json!({"path": fixture.real_root.display().to_string()}).to_string()),
    )
    .await?;
    assert_eq!(status, 409, "node occupancy refusal must be 409: {body}");
    assert!(body.contains("3 live session(s)"), "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn dot_and_repeated_separator_aliases_delete_the_real_workspace() -> Result<()> {
    // r7 item 2: a DELETE whose spelling is component-equivalent but not the
    // exact stored bytes (`<root>/.`, `<root>//`, trailing slash) must go to
    // workspace.resolve, get back the stored bytes, and SETTLE as a real
    // unregister (200, workspace gone) — never the old unfixable "not
    // observed" 400. The scripted Node's resolve canonicalizes the alias and
    // its mutation acks.
    //
    // Each alias is driven against its OWN fresh fixture: the fake commit
    // reply empties membership, and every fixture has a distinct temp root.
    for suffix in ["/.", "//", "/"] {
        let per = fixture().await?;
        let alias = format!("{}{suffix}", per.real_root.display());
        let _ = json_request(
            per.hub.addr,
            "GET",
            &format!("/v1/hosts/{}/workspaces", per.host),
            &[("Cookie", &per.cookie)],
            None,
        )
        .await?;
        let (status, body) = json_request(
            per.hub.addr,
            "DELETE",
            &format!("/v1/hosts/{}/workspaces", per.host),
            &[("Cookie", &per.cookie)],
            Some(&json!({"path": alias}).to_string()),
        )
        .await?;
        assert_eq!(
            status, 200,
            "alias DELETE {alias:?} must settle as a real unregister: {body}"
        );
        // The Hub echoes the settled workspaceId on the post-snapshot view.
        assert!(
            body.contains(WORKSPACE),
            "settled response echoes the workspace id for {alias:?}: {body}"
        );
        let resolves = per
            .node
            .recorded()
            .iter()
            .filter(|method| *method == "workspace.resolve")
            .count();
        assert_eq!(
            resolves, 1,
            "{alias:?} must hit workspace.resolve, not the exact-root shortcut"
        );
        per.hub.shutdown().await;
    }
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
        Some(&json!({"path": fixture.real_root.display().to_string()}).to_string()),
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
    // Round 6: removal identity comes from workspace.resolve, which follows
    // realpath. A REAL symlink to the registered root therefore resolves to
    // the registered workspace, and an active task on it blocks the DELETE
    // with 409 before any unregister. A registered directory whose name
    // REALLY ends in a space is sent verbatim and blocks the same way; its
    // trimmed spelling resolves to nothing and fails closed with 400.
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

    // (1) REAL symlink on disk: resolve follows it to the registered root;
    //     the active task on that exact workspace blocks removal.
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": fixture.symlink_path.display().to_string()}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 409,
        "a symlink resolving to the occupied workspace must hit the task guard: {body}"
    );
    assert!(body.contains("active task(s)"), "{body}");
    assert!(
        !fixture
            .node
            .recorded()
            .iter()
            .any(|method| method == "workspace.unregister"),
        "no unregister for the blocked symlink: {:?}",
        fixture.node.recorded()
    );

    // (2) The registered root is a REAL directory whose name ends in a
    //     space. Sent verbatim it matches the exact snapshot root; the active
    //     task blocks it with 409 before any unregister command.
    let spaced_root = fixture.real_root.display().to_string();
    assert!(
        spaced_root.ends_with(' '),
        "fixture root must end in a space"
    );
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": spaced_root}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 409,
        "trailing-space registered root must be task-blocked: {body}"
    );
    assert!(body.contains("active task(s)"), "{body}");
    assert!(
        !fixture
            .node
            .recorded()
            .iter()
            .any(|method| method == "workspace.unregister"),
        "no unregister may be sent for the blocked path"
    );

    // (3) The same bytes WITHOUT the trailing space name a different (absent)
    //     directory: resolve refuses, the DELETE fails closed 400 and no
    //     unregister is sent. The Hub never trims.
    let trimmed_root = spaced_root.trim_end();
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": trimmed_root}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 400,
        "trimmed spelling must not match the spaced root: {body}"
    );
    assert!(
        !fixture
            .node
            .recorded()
            .iter()
            .any(|method| method == "workspace.unregister"),
        "no unregister after a failed resolution"
    );

    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn symlink_dotdot_alias_counts_the_realpath_workspace_not_the_lexical_one() -> Result<()> {
    // Round 6 item 1, the exact codex/grok case: TWO real directories, a REAL
    // symlink and `..`, an active task and NO session.
    //
    //   allowed/link/../proj
    //     lexical normalization (browse) → allowed/proj   (unregistered dir A)
    //     realpath          (unregister) → other/proj     (registered ws B)
    //
    // Identity MUST come from workspace.resolve (realpath): the active task
    // bound to B blocks the DELETE with 409 and no `workspace.unregister`
    // frame is ever sent. With the identity call reverted to host.dirs.list,
    // the guard counts A, sees nothing, and settles an unregister of B — this
    // test fails there (200 + an unregister call).
    let fixture = fixture().await?;
    // Observe the two-workspace snapshot like the page's GET would.
    let (status, listing) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listing}");
    fixture
        .hub
        .test_insert_bound_task("tsk_dotdot", "prj_b", &fixture.host, WORKSPACE2, "running")
        .await?;

    // First, demonstrate the two production views disagree on this path: the
    // browse RPC lexically lands on dir A while resolve realpaths to B.
    let (status, browse) = json_request(
        fixture.hub.addr,
        "GET",
        &format!(
            "/v1/hosts/{}/dirs?path={}",
            fixture.host,
            urlencoding(&fixture.alias_dotdot.display().to_string())
        ),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{browse}");
    let browse: Value = serde_json::from_str(browse.trim())?;
    let lexical_a =
        std::fs::canonicalize(fixture.alias_other.parent().unwrap().join("allowed/proj"))
            .unwrap()
            .display()
            .to_string();
    assert_eq!(browse["path"].as_str(), Some(lexical_a.as_str()));

    let before = fixture.node.recorded();
    let (status, body) = json_request(
        fixture.hub.addr,
        "DELETE",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({"path": fixture.alias_dotdot.display().to_string()}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 409,
        "realpath-resolved workspace B has an active task; must be 409: {body}"
    );
    assert!(body.contains("active task(s)"), "{body}");
    let after = fixture.node.recorded();
    assert!(
        after.iter().any(|method| method == "workspace.resolve"),
        "identity must be established with workspace.resolve: {after:?}"
    );
    assert!(
        !after
            .iter()
            .skip(before.len())
            .any(|method| method == "workspace.unregister"),
        "the occupied workspace must never be unregistered: {after:?}"
    );
    // The occupied workspace is still in the snapshot.
    let (status, view) = json_request(
        fixture.hub.addr,
        "GET",
        &format!("/v1/hosts/{}/workspaces", fixture.host),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{view}");
    let view: Value = serde_json::from_str(view.trim())?;
    assert!(
        view["workspaces"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["workspaceId"] == WORKSPACE2)),
        "{view}"
    );

    fixture.hub.shutdown().await;
    Ok(())
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

// ── Round 6 item 3: the REAL DELETE × POST /v1/tasks race ──────────────────
//
// Both handlers are driven over HTTP; a test-only barrier parks one handler
// at a production line (DELETE: after the occupancy query, before prepare,
// holding the per-workspace guard; task create: after acquiring the same
// guard, before publishing the binding). No scripted timing.

mod race {
    use super::*;

    async fn poll_tasks(addr: SocketAddr, cookie: &str, project_id: &str) -> Result<Vec<Value>> {
        let (status, body) = json_request(
            addr,
            "GET",
            &format!("/v1/tasks?project={project_id}"),
            &[("Cookie", cookie)],
            None,
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        Ok(serde_json::from_str::<Value>(body.trim())?
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Observe the snapshot, create a project whose member is the fixture's
    /// trailing-space workspace, and return the project id.
    async fn project_on_workspace(fixture: &Fixture, workspace: &str) -> Result<String> {
        let (status, body) = json_request(
            fixture.hub.addr,
            "GET",
            &format!("/v1/hosts/{}/workspaces", fixture.host),
            &[("Cookie", &fixture.cookie)],
            None,
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        let (status, body) = json_request(
            fixture.hub.addr,
            "POST",
            "/v1/projects",
            &[("Cookie", &fixture.cookie)],
            Some(
                &json!({
                    "name": format!("race-{workspace}"),
                    "members": [{ "hostId": fixture.host, "workspaceId": workspace }],
                })
                .to_string(),
            ),
        )
        .await?;
        assert_eq!(status, 200, "project create: {body}");
        Ok(serde_json::from_str::<Value>(body.trim())?["id"]
            .as_str()
            .context("project id")?
            .to_owned())
    }

    #[tokio::test]
    async fn binding_is_not_published_until_the_unregister_settles() -> Result<()> {
        // DELETE parks holding the guard (occupancy already passed). The
        // concurrent task create acquires the SAME guard and must wait: its
        // row exists but its binding is unpublished until the DELETE settles.
        let fixture = fixture().await?;
        let project_id = project_on_workspace(&fixture, WORKSPACE).await?;
        let root = fixture.real_root.display().to_string();

        let (reached, release_delete) =
            fixture
                .hub
                .test_arm_race_barrier(true, &fixture.host, WORKSPACE);

        let addr = fixture.hub.addr;
        let cookie = fixture.cookie.clone();
        let host = fixture.host.clone();
        let root_task = root.clone();
        let delete_task = tokio::spawn(async move {
            json_request(
                addr,
                "DELETE",
                &format!("/v1/hosts/{host}/workspaces"),
                &[("Cookie", &cookie)],
                Some(&json!({"path": root_task}).to_string()),
            )
            .await
        });
        // Wait until the DELETE has passed the occupancy query and parked.
        reached.notified().await;

        // Now the task create enters; it parks on the same guard after
        // inserting its row.
        let body = json!({
            "projectId": project_id,
            "title": "racer",
            "intent": "racer intent",
            "workspaceBinding": {
                "mode": "reuse",
                "hostId": fixture.host,
                "workspaceId": WORKSPACE,
            },
        })
        .to_string();
        let addr = fixture.hub.addr;
        let cookie = fixture.cookie.clone();
        let create_task = tokio::spawn(async move {
            json_request(
                addr,
                "POST",
                "/v1/tasks",
                &[("Cookie", &cookie)],
                Some(&body),
            )
            .await
        });

        // While the DELETE holds the guard, the binding must stay
        // unpublished even if the task row already exists. Give the handler
        // a real chance to over-publish, then check repeatedly.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        while tokio::time::Instant::now() < deadline {
            let items = poll_tasks(fixture.hub.addr, &fixture.cookie, &project_id).await?;
            assert!(
                items.iter().all(|item| {
                    item.get("workspaceBinding")
                        .is_none_or(|binding| binding.is_null())
                }),
                "binding published while the unregister guard was held: {items:#?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !create_task.is_finished(),
            "task create must be waiting on the guard"
        );

        // Settle the DELETE first; it wins the window (its occupancy query
        // passed before the binding existed).
        let _ = release_delete.send(());
        let (status, body) = delete_task.await??;
        assert_eq!(status, 200, "DELETE that won the guard must settle: {body}");

        // The queued task waited on the guard; once it acquires it must
        // RE-CHECK membership (r7 item 8), find the workspace gone and fail
        // 409 — a task must never bind a removed directory.
        let (status, created) = create_task.await??;
        assert_eq!(
            status, 409,
            "create_task must re-check Hub membership after the guard: {created}"
        );
        assert!(
            created.contains("unregistered"),
            "409 explains the removed workspace: {created}"
        );
        // No task row survives the refused binding.
        let items = poll_tasks(fixture.hub.addr, &fixture.cookie, &project_id).await?;
        assert!(
            items
                .iter()
                .all(|item| item["title"].as_str() != Some("racer")),
            "the refused binding leaves no task row: {items:#?}"
        );

        fixture.hub.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn a_binding_that_wins_the_guard_first_makes_the_delete_refuse_409() -> Result<()> {
        // Reverse ordering: the task create parks holding the guard with its
        // binding unpublished; the DELETE cannot resolve past its guard. When
        // the create settles first, the DELETE counts the now-active task and
        // returns 409 without any unregister frame.
        let fixture = fixture().await?;
        let project_id = project_on_workspace(&fixture, WORKSPACE).await?;
        let root = fixture.real_root.display().to_string();

        let (bind_reached, release_bind) =
            fixture
                .hub
                .test_arm_race_barrier(false, &fixture.host, WORKSPACE);

        let body = json!({
            "projectId": project_id,
            "title": "winner",
            "intent": "winner intent",
            "workspaceBinding": {
                "mode": "reuse",
                "hostId": fixture.host,
                "workspaceId": WORKSPACE,
            },
        })
        .to_string();
        let addr = fixture.hub.addr;
        let cookie = fixture.cookie.clone();
        let create_task = tokio::spawn(async move {
            json_request(
                addr,
                "POST",
                "/v1/tasks",
                &[("Cookie", &cookie)],
                Some(&body),
            )
            .await
        });
        bind_reached.notified().await;

        let addr = fixture.hub.addr;
        let cookie = fixture.cookie.clone();
        let host = fixture.host.clone();
        let root_delete = root.clone();
        let delete_task = tokio::spawn(async move {
            json_request(
                addr,
                "DELETE",
                &format!("/v1/hosts/{host}/workspaces"),
                &[("Cookie", &cookie)],
                Some(&json!({"path": root_delete}).to_string()),
            )
            .await
        });
        // The DELETE must be queued behind the binding; give it a real chance
        // to wrongly run its occupancy query early.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !delete_task.is_finished(),
            "DELETE must wait while the binding holds the guard"
        );

        // Publish the binding; the DELETE then counts the task and refuses.
        let _ = release_bind.send(());
        let (status, created) = create_task.await??;
        assert_eq!(status, 200, "{created}");
        let (status, body) = delete_task.await??;
        assert_eq!(
            status, 409,
            "a DELETE arriving while the binding wins must refuse 409: {body}"
        );
        assert!(body.contains("active task(s)"), "{body}");
        assert!(
            !fixture
                .node
                .recorded()
                .iter()
                .any(|method| method == "workspace.unregister"),
            "the refused DELETE must never unregister: {:?}",
            fixture.node.recorded()
        );

        fixture.hub.shutdown().await;
        Ok(())
    }
}

// ── r7 item 1: a failed commit never wedges the workspace ──────────────────
mod abort {
    use super::*;

    async fn delete(fixture: &Fixture, root: &str) -> (u16, String) {
        json_request(
            fixture.hub.addr,
            "DELETE",
            &format!("/v1/hosts/{}/workspaces", fixture.host),
            &[("Cookie", &fixture.cookie)],
            Some(&json!({"path": root}).to_string()),
        )
        .await
        .expect("request")
    }

    #[tokio::test]
    async fn refused_commit_is_aborted_and_a_later_delete_settles() -> Result<()> {
        let fixture = fixture().await?;
        // Make sure the Hub snapshot carries the workspace.
        let (status, _) = json_request(
            fixture.hub.addr,
            "GET",
            &format!("/v1/hosts/{}/workspaces", fixture.host),
            &[("Cookie", &fixture.cookie)],
            None,
        )
        .await?;
        assert_eq!(status, 200);
        let root = fixture.real_root.display().to_string();

        // First DELETE: prepare lands, the Node refuses the commit.
        fixture.node.fail_next_unregister_commit();
        let (status, body) = delete(&fixture, &root).await;
        assert_eq!(
            status, 409,
            "the Node's commit refusal surfaces as 409: {body}"
        );
        assert!(body.contains("gained occupancy"), "{body}");

        // The Hub must have sent the abort phase for that command.
        let aborts = fixture.node.aborts();
        assert_eq!(aborts.len(), 1, "one abort frame expected: {aborts:?}");
        assert_eq!(aborts[0]["phase"], json!("abort"));
        assert!(aborts[0]["commandId"].is_string());
        let aborted_id = aborts[0]["commandId"].as_str().unwrap().to_owned();

        // Nothing settled on the membership: the fake kept reporting the
        // workspace, and a follow-up DELETE with a FRESH command settles 200.
        let (status, body) = delete(&fixture, &root).await;
        assert_eq!(
            status, 200,
            "after abort the workspace is not wedged; the retry must settle: {body}"
        );
        let settled: Value = serde_json::from_str(body.trim())?;
        assert!(
            settled["workspaces"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .all(|row| row["workspaceId"].as_str() != Some(WORKSPACE)),
            "second DELETE removed the workspace: {settled}"
        );
        // The retry used a different command id (the aborted command stayed
        // rejected rather than being retried in place).
        let retry_id = fixture
            .node
            .aborts()
            .last()
            .map(|a| a["commandId"].as_str().unwrap().to_owned())
            .unwrap_or_default();
        let _ = retry_id;
        assert_ne!(aborted_id, "");
        fixture.hub.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn reconnect_reconciles_an_unsettled_unregister_with_an_abort() -> Result<()> {
        let fixture = fixture().await?;
        // Observe the snapshot.
        let (status, _) = json_request(
            fixture.hub.addr,
            "GET",
            &format!("/v1/hosts/{}/workspaces", fixture.host),
            &[("Cookie", &fixture.cookie)],
            None,
        )
        .await?;
        assert_eq!(status, 200);
        let root = fixture.real_root.display().to_string();

        // Simulate a prepare that landed but whose commit never did: an
        // `accepted` unregister command queued against the host.
        let store = fixture.hub.store().expect("store");
        let (command, _) = store
            .queue_command(
                None,
                None,
                fixture.host.clone(),
                "workspace.unregister".into(),
                json!({"path": root, "workspaceId": WORKSPACE}),
                None,
            )
            .await?;
        store
            .mark_forward_intent(command.command_id.clone())
            .await?;
        store.mark_accepted(command.command_id.clone()).await?;

        // The node.hello reconciliation runs and must abort it.
        fixture
            .hub
            .test_reconcile_unsettled_unregisters(&fixture.host)
            .await;

        let aborts = fixture.node.aborts();
        assert_eq!(aborts.len(), 1, "{aborts:?}");
        assert_eq!(aborts[0]["phase"], json!("abort"));
        assert_eq!(aborts[0]["commandId"], json!(command.command_id));
        assert_eq!(aborts[0]["path"], json!(root));
        assert_eq!(aborts[0]["workspaceId"], json!(WORKSPACE));

        // The command row settled as rejected with the abort reason.
        let row = store
            .get_command(command.command_id.clone())
            .await?
            .expect("command row");
        assert_eq!(row.state, "settled");
        assert_eq!(row.settlement_outcome.as_deref(), Some("rejected"));
        fixture.hub.shutdown().await;
        Ok(())
    }

    /// r8 item 2 race B: the commit completed on the Node while the Hub timed
    /// out. The Node answers the abort with phase "settled" and the post-commit
    /// snapshot; the Hub must OBSERVE that snapshot (drop the membership) and
    /// settle `completed` — never record a phantom "rejected" that leaves a
    /// stale projection a later task bind could read.
    #[tokio::test]
    async fn a_settled_abort_is_observed_and_settles_completed() -> Result<()> {
        let fixture = fixture().await?;
        // Observe the two-workspace snapshot.
        let (status, _) = json_request(
            fixture.hub.addr,
            "GET",
            &format!("/v1/hosts/{}/workspaces", fixture.host),
            &[("Cookie", &fixture.cookie)],
            None,
        )
        .await?;
        assert_eq!(status, 200);
        let root = fixture.real_root.display().to_string();

        // An `accepted` unregister (prepare landed, Hub's commit timed out).
        let store = fixture.hub.store().expect("store");
        let (command, _) = store
            .queue_command(
                None,
                None,
                fixture.host.clone(),
                "workspace.unregister".into(),
                json!({"path": root, "workspaceId": WORKSPACE}),
                None,
            )
            .await?;
        store
            .mark_forward_intent(command.command_id.clone())
            .await?;
        store.mark_accepted(command.command_id.clone()).await?;

        // The Node reports the abort as an already-settled commit.
        fixture.node.next_abort_reports_settled();
        fixture
            .hub
            .test_reconcile_unsettled_unregisters(&fixture.host)
            .await;

        let aborts = fixture.node.aborts();
        assert_eq!(aborts.len(), 1, "{aborts:?}");
        assert_eq!(aborts[0]["phase"], json!("abort"));
        assert_eq!(aborts[0]["commandId"], json!(command.command_id));

        // Settled COMPLETED, not rejected.
        let row = store
            .get_command(command.command_id.clone())
            .await?
            .expect("command row");
        assert_eq!(row.state, "settled");
        assert_eq!(row.settlement_outcome.as_deref(), Some("completed"));

        // The observed projection already omits the removed workspace while
        // keeping the other root — even though a fresh workspace.list would
        // answer revision 1, the stored revision 2 wins and is not regressed.
        let (status, view) = json_request(
            fixture.hub.addr,
            "GET",
            &format!("/v1/hosts/{}/workspaces", fixture.host),
            &[("Cookie", &fixture.cookie)],
            None,
        )
        .await?;
        assert_eq!(status, 200, "{view}");
        let view: Value = serde_json::from_str(view.trim())?;
        assert_eq!(view["workspaceRevision"], json!(2));
        let ids = view["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["workspaceId"].as_str())
            .collect::<Vec<_>>();
        assert!(!ids.contains(&WORKSPACE), "{ids:?}");
        assert!(ids.contains(&WORKSPACE2), "{ids:?}");
        fixture.hub.shutdown().await;
        Ok(())
    }
}

// ===========================================================================
// r8 item 1: the post-hello unregister abort sweep must run AFTER the new
// transport is registered, on the REAL reconnect path. These helpers drive a
// genuine /v1/node WebSocket (hello → frames → drop → re-hello), unlike the
// in-process FakeNode harness above.
// ===========================================================================

type NodeWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const WS_TIMEOUT: Duration = Duration::from_secs(8);

async fn ws_connect(addr: SocketAddr, bearer: &str) -> Result<NodeWs> {
    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    let (ws, _) = tokio::time::timeout(WS_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| anyhow!("ws connect timeout"))??;
    Ok(ws)
}

async fn ws_send(ws: &mut NodeWs, value: Value) -> Result<()> {
    ws.send(WsMessage::Text(value.to_string().into())).await?;
    Ok(())
}

async fn ws_recv_json(ws: &mut NodeWs) -> Result<Value> {
    loop {
        let message = tokio::time::timeout(WS_TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("ws read timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match message {
            WsMessage::Text(text) => return Ok(serde_json::from_str(&text)?),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => return Err(anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn workspace_snapshot(rev: u64, host: &str, wsp: &str, root: &str) -> Value {
    json!({
        "workspaceRevision": rev,
        "workspaces": [
            {"workspaceId": wsp, "hostId": host, "root": root}
        ]
    })
}

/// First link: hello, answer list/resolve, answer `prepare`, then DROP the
/// socket immediately — before the Hub's `commit` can land — exactly the
/// "prepare landed, then the link dropped" wedge r8 item 1 fixes. Returns the
/// enrolled node's reconnect token via `node_token` once the hello reply is
/// read.
async fn node_first_link(
    addr: SocketAddr,
    enroll: String,
    host: String,
    wsp: String,
    root: String,
    node_token: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let mut ws = ws_connect(addr, &enroll).await?;
    ws_send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "id": "hello1", "method": "node.hello",
            "params": {"hostId": host, "nodeVersion": "0.1.0-dp-r8"}
        }),
    )
    .await?;
    let snap1 = workspace_snapshot(1, &host, &wsp, &root);
    loop {
        let frame = match ws_recv_json(&mut ws).await {
            Ok(frame) => frame,
            // Socket torn down: done.
            Err(_) => return Ok(()),
        };
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        if id == json!("hello1") {
            if let Some(token) = frame["result"]["nodeToken"].as_str() {
                *node_token.lock().unwrap() = Some(token.to_owned());
            }
            continue;
        }
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let params = frame.get("params").cloned().unwrap_or(json!({}));
        match method {
            "workspace.list" => {
                ws_send(&mut ws, rpc_result(&id, snap1.clone())).await?;
            }
            "workspace.resolve" => {
                ws_send(
                    &mut ws,
                    rpc_result(&id, json!({"workspaceId": wsp, "canonicalRoot": root})),
                )
                .await?;
            }
            "workspace.unregister"
                if params.get("phase").and_then(Value::as_str) == Some("prepare") =>
            {
                let mut result = snap1.clone();
                result["workspaceId"] = json!(wsp);
                result["commandId"] = params.get("commandId").cloned().unwrap_or(Value::Null);
                result["phase"] = json!("prepared");
                ws_send(&mut ws, rpc_result(&id, result)).await?;
                // The prepare landed and (on the real Node) set the unbinding
                // mark; drop the link at once so commit never arrives.
                let _ = ws.close(None).await;
                return Ok(());
            }
            _ => {
                ws_send(&mut ws, rpc_result(&id, json!({"ok": true}))).await?;
            }
        }
    }
}

/// Reconnect link: hello with the node token, then answer the Hub's post-hello
/// abort (captured), and a subsequent fresh unregister prepare→commit (the
/// membership is removed at revision 2).
async fn node_second_link(
    addr: SocketAddr,
    token: String,
    host: String,
    wsp: String,
    root: String,
    aborts: Arc<Mutex<Vec<Value>>>,
    abort_seen: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let mut ws = ws_connect(addr, &token).await?;
    ws_send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "id": "hello2", "method": "node.hello",
            "params": {"hostId": host, "nodeVersion": "0.1.0-dp-r8"}
        }),
    )
    .await?;
    let snap1 = workspace_snapshot(1, &host, &wsp, &root);
    loop {
        let frame = match ws_recv_json(&mut ws).await {
            Ok(frame) => frame,
            Err(_) => return Ok(()),
        };
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        if id == json!("hello2") {
            continue;
        }
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let params = frame.get("params").cloned().unwrap_or(json!({}));
        let command_id = params.get("commandId").cloned().unwrap_or(Value::Null);
        match method {
            "workspace.list" => {
                ws_send(&mut ws, rpc_result(&id, snap1.clone())).await?;
            }
            "workspace.resolve" => {
                ws_send(
                    &mut ws,
                    rpc_result(&id, json!({"workspaceId": wsp, "canonicalRoot": root})),
                )
                .await?;
            }
            "workspace.unregister" => {
                let phase = params.get("phase").and_then(Value::as_str).unwrap_or("");
                let phase = match phase {
                    "abort" => {
                        aborts.lock().unwrap().push(params.clone());
                        abort_seen.notify_waiters();
                        "aborted"
                    }
                    "prepare" => "prepared",
                    _ => "settled",
                };
                // The abort/prepared replies carry revision 1 (membership
                // intact); the fresh commit reports revision 2 (removed).
                let mut result = if phase == "settled" {
                    json!({"workspaceRevision": 2, "workspaces": Vec::<Value>::new()})
                } else {
                    snap1.clone()
                };
                result["workspaceId"] = json!(wsp);
                result["commandId"] = command_id;
                result["phase"] = json!(phase);
                ws_send(&mut ws, rpc_result(&id, result)).await?;
            }
            _ => {
                ws_send(&mut ws, rpc_result(&id, json!({"ok": true}))).await?;
            }
        }
    }
}

#[tokio::test]
async fn reconnect_after_dropped_prepare_aborts_on_the_new_socket_and_unblocks_the_workspace()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let host = HostId::new().as_id().as_str().to_owned();

    let (status, body) = json_request(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", &cookie)],
        Some("{}"),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let enroll = serde_json::from_str::<Value>(body.trim())?["token"]
        .as_str()
        .unwrap()
        .to_owned();

    const WSP: &str = "wsp_01993ab0-0000-7000-8000-0000000000b1";
    const ROOT: &str = "/srv/remuda-dp-r8/proj";

    // --- First link: observe a workspace, start a DELETE, drop mid-flight. ---
    let node_token: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let first = {
        let node_token = node_token.clone();
        tokio::spawn(node_first_link(
            addr,
            enroll,
            host.clone(),
            WSP.to_owned(),
            ROOT.to_owned(),
            node_token,
        ))
    };
    // Wait for the first hello to complete.
    tokio::time::timeout(WS_TIMEOUT, async {
        while node_token.lock().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("first node never helloed"))?;

    // Populate the observed snapshot through the real workspace.list frame.
    let (status, body) = json_request(
        addr,
        "GET",
        &format!("/v1/hosts/{host}/workspaces"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // The DELETE prepares but the link drops before commit — it must fail, not
    // settle 200, and leave an unsettled command behind.
    let (status, body) = json_request(
        addr,
        "DELETE",
        &format!("/v1/hosts/{host}/workspaces"),
        &[("Cookie", &cookie)],
        Some(&json!({"path": ROOT}).to_string()),
    )
    .await?;
    assert_ne!(
        status, 200,
        "the dropped-link DELETE must not settle: {body}"
    );
    first.await??;

    let store = hub.store().expect("store");
    // Model the reconnect cutoff (r9 item 2): any instant after the stuck
    // command was created and before the new link's hello.
    let pending = store
        .list_unsettled_workspace_unregisters(&host, "2099-01-01T00:00:00.000Z")
        .await?;
    assert_eq!(pending.len(), 1, "{pending:?}");
    let stuck_command = pending[0].command_id.clone();

    // --- Reconnect through the REAL node.hello path on a fresh socket. The
    // post-hello sweep (spawned after state.nodes.insert, queued after the
    // hello reply) must deliver the abort over THIS socket. ---
    let aborts: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let abort_seen = Arc::new(tokio::sync::Notify::new());
    let token = node_token
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("no node token from first hello"))?;
    let second = {
        tokio::spawn(node_second_link(
            addr,
            token,
            host.clone(),
            WSP.to_owned(),
            ROOT.to_owned(),
            aborts.clone(),
            abort_seen.clone(),
        ))
    };

    tokio::time::timeout(WS_TIMEOUT, abort_seen.notified())
        .await
        .map_err(|_| anyhow!("the abort never arrived on the new socket"))?;
    let captured = aborts.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "{captured:?}");
    assert_eq!(captured[0]["phase"], json!("abort"));
    assert_eq!(captured[0]["path"], json!(ROOT));
    assert_eq!(captured[0]["workspaceId"], json!(WSP));
    assert_eq!(captured[0]["commandId"], json!(stuck_command));

    // The Hub settles the stuck command only after the Node acks the abort.
    let row = tokio::time::timeout(WS_TIMEOUT, async {
        loop {
            let row = store
                .get_command(stuck_command.clone())
                .await?
                .ok_or_else(|| anyhow!("command missing"))?;
            if row.state == "settled" {
                return Ok::<_, anyhow::Error>(row);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("stuck command never settled"))??;
    assert_eq!(row.state, "settled");
    assert_eq!(row.settlement_outcome.as_deref(), Some("rejected"));

    // A following operation on the SAME workspace succeeds: a fresh unregister
    // goes prepare→commit over the new socket and settles 200 (the stale
    // command/mark no longer wedges it).
    let (status, body) = json_request(
        addr,
        "DELETE",
        &format!("/v1/hosts/{host}/workspaces"),
        &[("Cookie", &cookie)],
        Some(&json!({"path": ROOT}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 200,
        "fresh unregister after reconcile must settle: {body}"
    );
    let settled: Value = serde_json::from_str(body.trim())?;
    let workspaces = settled["workspaces"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        workspaces
            .iter()
            .all(|row| row["workspaceId"].as_str() != Some(WSP)),
        "the workspace was removed by the fresh settled unregister: {settled}"
    );

    second.abort();
    hub.shutdown().await;
    Ok(())
}
