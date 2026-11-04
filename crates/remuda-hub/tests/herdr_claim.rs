//! D-062 Hub routes for claiming an external herdr pane:
//! - `GET /v1/hosts/{id}/herdr` proxies `herdr.discover`;
//! - `POST /v1/instances/claim` resolves on the Node (read-only), indexes a
//!   `herdr-claimed` row and commits `instance.claim`;
//! - `POST /v1/instances/{id}/release` detaches (Node `instance.close`).
//!
//! Every route is Human-operator only: Agent, Bot and narrowed callers get 403
//! with a reason and a next step, and the refusal never reaches the Node.
//! The Node side (fake herdr) is covered by `remuda-node`'s `herdr_claim`
//! tests; this scripted Node answers like it.

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

const SOCKET: &str = "/run/user/herdr/sessions/work/herdr.sock";
/// The registered workspace the scripted Node resolves the pane's cwd to.
const WORKSPACE: &str = "wsp_01993ab0-0000-7000-8000-0000000000a1";

/// Scripted Node answering the claim RPCs the way `remuda-node` does,
/// recording every method and its params.
#[derive(Default)]
struct FakeNode {
    calls: Mutex<Vec<(String, Value)>>,
    /// D-062 r2 item 4: commit the claim, then lose the reply.
    drop_claim_reply: std::sync::atomic::AtomicBool,
    /// D-062 r3 item 1: commit the claim, then the link dies mid-call — the
    /// transport is unregistered before the Hub sees the (lost) reply.
    vanish_on_claim: Mutex<Option<(remuda_hub::HubTestHandle, String)>>,
    /// The Node's live claims: instance id → pane id; a close frees one.
    held: Mutex<std::collections::HashMap<String, String>>,
    /// Refuse a resolve of a held pane like the real Node's uniqueness check
    /// (off by default, so the Hub's own check is what other tests see).
    node_uniqueness: std::sync::atomic::AtomicBool,
    /// Refuse this many `instance.close` frames with an explicit error (the
    /// Node did not run them) before accepting.
    refuse_closes: std::sync::atomic::AtomicUsize,
}

impl FakeNode {
    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(method, _)| method.clone())
            .collect()
    }

    fn params_of(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, _)| seen == method)
            .map(|(_, params)| params.clone())
            .collect()
    }
}

fn node_error(message: &str) -> Value {
    json!({"error": {"code": -32602, "message": message}})
}

fn resolution(params: &Value) -> Value {
    let pane = params
        .get("paneId")
        .and_then(Value::as_str)
        .unwrap_or("w1:p1");
    json!({"result": {
        "claim": {
            "session": params["session"], "socketPath": SOCKET,
            "workspaceId": params["workspaceId"], "tabId": params["tabId"], "paneId": pane,
            "agent": "claude", "process": {"pid": 4242, "command": "claude"}, "contact": "ok",
        },
        "kind": "claude",
        "agentStatus": "idle",
        "remudaWorkspaceId": WORKSPACE,
    }})
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
            self.calls
                .lock()
                .unwrap()
                .push((method.clone(), params.clone()));
            let pane = params
                .get("paneId")
                .and_then(Value::as_str)
                .unwrap_or("w1:p1")
                .to_owned();
            let instance = params
                .get("instanceId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if method == "instance.close"
                && self
                    .refuse_closes
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |left| left.checked_sub(1),
                    )
                    .is_ok()
            {
                return Ok(Some(node_error("node busy: try the close again")));
            }
            match method.as_str() {
                "instance.claim" => {
                    self.held.lock().unwrap().insert(instance, pane.clone());
                }
                "instance.close" => {
                    self.held.lock().unwrap().remove(&instance);
                }
                _ => {}
            }
            if method == "instance.claim" {
                let vanish = self.vanish_on_claim.lock().unwrap().take();
                if let Some((hub, host)) = vanish {
                    hub.test_disconnect_node(&host).await;
                    return Err(remuda_hub::HubError::Internal(
                        "node link closed before the reply".into(),
                    ));
                }
            }
            if method == "instance.claim"
                && self
                    .drop_claim_reply
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(remuda_hub::HubError::Internal(
                    "node link dropped before the reply".into(),
                ));
            }
            let holder = self
                .held
                .lock()
                .unwrap()
                .iter()
                .find(|(_, held)| **held == pane)
                .map(|(id, _)| id.clone());
            if method == "herdr.resolve"
                && self
                    .node_uniqueness
                    .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(holder) = holder
            {
                return Ok(Some(node_error(&format!(
                    "conflict: herdr-claim-conflict: pane {pane} in herdr session work is already claimed by {holder}"
                ))));
            }
            let reply = match method.as_str() {
                "herdr.discover" => json!({"result": {"sessions": [{
                    "session": "work", "socketPath": SOCKET, "reachable": true,
                    "workspaces": [{"workspaceId": "w1", "label": "human-work", "tabs": [
                        {"tabId": "w1:t1", "label": "1", "panes": [
                            {"paneId": "w1:p1", "agent": "claude", "agentStatus": "idle",
                             "process": {"pid": 4242, "command": "claude"}}]}]}],
                }]}}),
                "herdr.resolve" | "instance.claim" => {
                    if params["workspaceId"] == "w9" {
                        node_error(
                            "invalid request: herdr-claim-invalid: workspace w9 is not in herdr session work",
                        )
                    } else if params["tabId"] == "w1:t2" && params.get("paneId").is_none() {
                        node_error(
                            "invalid request: herdr-claim-ambiguous: tab w1:t2 has 2 panes; pass --pane with one of: w1:p2, w1:p3",
                        )
                    } else if params["session"] == "gone" {
                        node_error(
                            "invalid request: herdr-unreachable: herdr session gone did not answer: connection refused",
                        )
                    } else if method == "herdr.resolve" {
                        resolution(&params)
                    } else {
                        json!({"result": {"instance": {"id": params["instanceId"], "driver": "herdr-claimed"}}})
                    }
                }
                "instance.close" => json!({"result": {"accepted": true}}),
                other => node_error(&format!("unexpected {other}")),
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

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<(u16, Value)> {
    let body = body.map(Value::to_string);
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(text) = &body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            text.len()
        ));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    if let Some(text) = &body {
        bytes.extend_from_slice(text.as_bytes());
    }
    stream.write_all(&bytes).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("header terminator")?;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let text = String::from_utf8_lossy(&buf[split + 4..]).to_string();
    Ok((
        status,
        serde_json::from_str(text.trim()).unwrap_or(Value::String(text)),
    ))
}

async fn login(addr: SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({"bootstrapToken": bootstrap, "deviceName": "hclaim-phone"});
    let mut stream = TcpStream::connect(addr).await?;
    let text = body.to_string();
    stream
        .write_all(
            format!(
                "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
                text.len()
            )
            .as_bytes(),
        )
        .await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let head = String::from_utf8_lossy(&buf).to_string();
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':'))
        .and_then(|(_, value)| value.split(';').next())
        .map(|cookie| cookie.trim().to_owned())
        .context("set-cookie")
}

struct Fixture {
    hub: remuda_hub::RunningHub,
    cookie: String,
    node: Arc<FakeNode>,
    host: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let config = HubConfig::for_test(dir.path().join("data"));
        let bootstrap = config.bootstrap_token.clone();
        let hub = spawn(config).await?;
        let cookie = login(hub.addr, &bootstrap).await?;
        let host = HostId::new().as_id().as_str().to_owned();
        hub.test_insert_host(&host).await?;
        let node = Arc::new(FakeNode::default());
        hub.test_set_node_transport(&host, node.clone()).await;
        Ok(Self {
            hub,
            cookie,
            node,
            host,
            _dir: dir,
        })
    }

    fn human(&self) -> Vec<(&'static str, String)> {
        vec![("Cookie", self.cookie.clone())]
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&'static str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        request(self.hub.addr, method, path, &headers, body).await
    }

    fn claim_body(&self, tab: &str, pane: Option<&str>) -> Value {
        let mut body = json!({
            "hostId": self.host, "herdrSession": "work", "workspaceId": "w1", "tabId": tab,
            "name": "human claude",
        });
        if let Some(pane) = pane {
            body["paneId"] = json!(pane);
        }
        body
    }

    async fn audit(&self, instance_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .hub
            .store()
            .context("store")?
            .audit_for(instance_id.to_owned())
            .await?)
    }
}

#[tokio::test]
async fn human_claims_a_pane_and_the_record_says_herdr_claimed() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (status, listing) = fixture
        .call(
            "GET",
            &format!("/v1/hosts/{}/herdr?session=work", fixture.host),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{listing}");
    assert_eq!(
        listing["sessions"][0]["workspaces"][0]["tabs"][0]["panes"][0]["paneId"],
        "w1:p1"
    );
    assert_eq!(
        fixture.node.params_of("herdr.discover"),
        vec![json!({"session": "work"})]
    );

    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let instance = &body["instance"];
    let id = instance["instanceId"]
        .as_str()
        .context("instance id")?
        .to_owned();
    assert_eq!(instance["driver"], "herdr-claimed");
    assert_eq!(instance["kind"], "claude");
    assert_eq!(instance["lifecycle"], "ready");
    assert_eq!(instance["launchedBy"], "user");
    assert_eq!(instance["permissionMode"], "external");
    assert_eq!(instance["name"], "human claude");
    assert_eq!(
        instance["workspaceId"], WORKSPACE,
        "the row files under the workspace holding the pane, not 其他"
    );
    assert_eq!(instance["herdrClaim"]["session"], "work");
    assert_eq!(instance["herdrClaim"]["workspaceId"], "w1");
    assert_eq!(instance["herdrClaim"]["tabId"], "w1:t1");
    assert_eq!(instance["herdrClaim"]["paneId"], "w1:p1");
    assert_eq!(instance["herdrClaim"]["process"]["pid"], 4242);
    // Resolve first (read-only), then the commit carrying the Hub's id.
    assert_eq!(
        fixture.node.methods(),
        vec!["herdr.discover", "herdr.resolve", "instance.claim"]
    );
    assert_eq!(
        fixture.node.params_of("instance.claim")[0]["instanceId"],
        json!(id)
    );

    let audit = fixture.audit(&id).await?;
    let claim = audit
        .iter()
        .find(|row| row["action"] == "instance.claim")
        .context("claim audit row")?;
    let detail = &claim["detail"];
    for (key, want) in [
        ("hostId", json!(fixture.host)),
        ("session", json!("work")),
        ("socketPath", json!(SOCKET)),
        ("workspaceId", json!("w1")),
        ("tabId", json!("w1:t1")),
        ("paneId", json!("w1:p1")),
        ("pid", json!(4242)),
    ] {
        assert_eq!(detail[key], want, "audit {key}: {claim}");
    }
    assert!(
        claim["deviceId"].as_str().is_some_and(|id| !id.is_empty()),
        "{claim}"
    );
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn invalid_and_ambiguous_targets_are_400_and_create_nothing() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut missing = fixture.claim_body("w1:t1", None);
    missing["workspaceId"] = json!("w9");
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&missing),
        )
        .await?;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["reason"], "invalid-target", "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("workspace w9"),
        "{body}"
    );

    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t2", None)),
        )
        .await?;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["reason"], "pane-ambiguous", "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("w1:p2, w1:p3"),
        "the refusal lists the tab's pane ids: {body}"
    );

    let mut gone = fixture.claim_body("w1:t1", None);
    gone["herdrSession"] = json!("gone");
    let (status, body) = fixture
        .call("POST", "/v1/instances/claim", &fixture.human(), Some(&gone))
        .await?;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["reason"], "herdr-unreachable", "{body}");

    let (status, list) = fixture
        .call("GET", "/v1/instances", &fixture.human(), None)
        .await?;
    assert_eq!(status, 200);
    assert_eq!(list["items"], json!([]), "a refused claim indexes nothing");
    assert!(
        !fixture
            .node
            .methods()
            .iter()
            .any(|method| method == "instance.claim"),
        "a refused target never reaches the commit: {:?}",
        fixture.node.methods()
    );
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_duplicate_claim_is_409_naming_the_live_instance() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (status, first) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert_eq!(status, 200, "{first}");
    let id = first["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned();
    let (status, second) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", Some("w1:p1"))),
        )
        .await?;
    assert_eq!(status, 409, "{second}");
    assert_eq!(second["reason"], "already-claimed", "{second}");
    assert!(
        second["error"].as_str().unwrap_or("").contains(&id),
        "{second}"
    );
    assert_eq!(
        fixture.node.params_of("instance.claim").len(),
        1,
        "the duplicate never reaches the Node's commit"
    );

    // After a release the pane is claimable again.
    let (status, released) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{released}");
    let (status, again) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert_eq!(status, 200, "{again}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn agents_bots_and_narrowed_humans_are_403_with_reason_and_next() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (status, claimed) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert_eq!(status, 200, "{claimed}");
    let id = claimed["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned();
    let calls_before = fixture.node.methods().len();

    let agent = fixture
        .hub
        .test_mint_agent_token("hclaim-agent", &id)
        .await?;
    let bot = fixture.hub.mint_bot_device_token("hclaim-bot").await?;
    let callers: Vec<(&str, Vec<(&'static str, String)>)> = vec![
        ("agent", vec![("Authorization", format!("Bearer {agent}"))]),
        ("bot", vec![("Authorization", format!("Bearer {bot}"))]),
        (
            "narrowed human",
            vec![
                ("Cookie", fixture.cookie.clone()),
                ("x-remuda-instance-id", id.clone()),
            ],
        ),
    ];
    let claim = fixture.claim_body("w1:t1", Some("w1:p9"));
    let send = json!({"operation": "instance.send", "payload": {"input": {"type": "prompt", "text": "hi"}}});
    for (who, headers) in &callers {
        for (method, path, body) in [
            ("GET", format!("/v1/hosts/{}/herdr", fixture.host), None),
            ("POST", "/v1/instances/claim".to_owned(), Some(&claim)),
            ("POST", format!("/v1/instances/{id}/release"), None),
            ("POST", format!("/v1/instances/{id}/commands"), Some(&send)),
        ] {
            let (status, body) = fixture.call(method, &path, headers, body).await?;
            assert_eq!(status, 403, "{who} {method} {path}: {body}");
            assert!(
                body["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty())
                    && body["next"]["action"].as_str().is_some(),
                "{who} {method} {path} carries reason and next: {body}"
            );
        }
    }
    assert_eq!(
        fixture.node.methods().len(),
        calls_before,
        "refusals never reach the Node: {:?}",
        fixture.node.methods()
    );
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn release_detaches_once_is_idempotent_and_is_audited() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (_, claimed) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    let id = claimed["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned();
    let (status, body) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["released"], true);
    assert_eq!(body["instance"]["lifecycle"], "exited");
    assert_eq!(body["instance"]["lastError"], "released");
    let closes = fixture.node.params_of("instance.close");
    assert_eq!(closes.len(), 1, "release is the Node's detach: {closes:?}");
    assert_eq!(closes[0]["instanceId"], json!(id));

    let (status, again) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["released"], false, "a second release is a no-op");
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);

    let audit = fixture.audit(&id).await?;
    let releases: Vec<&Value> = audit
        .iter()
        .filter(|row| row["action"] == "instance.release")
        .collect();
    assert_eq!(releases.len(), 1, "{audit:?}");
    assert_eq!(releases[0]["detail"]["paneId"], "w1:p1");
    assert_eq!(releases[0]["detail"]["tabId"], "w1:t1");
    assert_eq!(releases[0]["detail"]["session"], "work");

    // Releasing an instance that is not a claim is refused.
    let (status, plain) = fixture
        .call(
            "POST",
            "/v1/instances/ins_missing/release",
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 404, "{plain}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_close_through_the_commands_route_is_an_audited_release() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (_, claimed) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    let id = claimed["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned();
    let (status, body) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/commands"),
            &fixture.human(),
            Some(&json!({"operation": "instance.close", "payload": {}})),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let audit = fixture.audit(&id).await?;
    assert!(
        audit
            .iter()
            .any(|row| row["action"] == "instance.release" && row["detail"]["paneId"] == "w1:p1"),
        "{audit:?}"
    );
    fixture.hub.shutdown().await;
    Ok(())
}

fn contact_event(state: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "observedAt": "2026-11-04T00:00:00.000Z",
        "payload": {
            "type": "native",
            "topic": "diagnostic",
            "nativeName": "herdr_claim_contact",
            "nativeId": { "state": "known", "value": "w1:p1" },
            "status": { "state": "known", "value": state },
            "severity": if state == "lost" { "warning" } else { "info" },
            "affectsCompletion": false,
            "dataRef": null,
            "relatedIds": { "session": "work" }
        }
    })
}

#[tokio::test]
async fn contact_loss_shows_on_the_claim_and_never_ends_the_instance() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (_, claimed) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    let id = claimed["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned();
    let store = fixture.hub.store().context("store")?;
    store
        .append_journal(
            fixture.host.clone(),
            id.clone(),
            Some(1),
            contact_event("lost"),
        )
        .await?;
    let (_, lost) = fixture
        .call(
            "GET",
            &format!("/v1/instances/{id}"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(lost["herdrClaim"]["contact"], "lost", "{lost}");
    assert_eq!(
        lost["lifecycle"], "ready",
        "contact loss is not process end: {lost}"
    );
    store
        .append_journal(
            fixture.host.clone(),
            id.clone(),
            Some(2),
            contact_event("ok"),
        )
        .await?;
    let (_, back) = fixture
        .call(
            "GET",
            &format!("/v1/instances/{id}"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(back["herdrClaim"]["contact"], "ok", "{back}");
    fixture.hub.shutdown().await;
    Ok(())
}

// ---- D-062 r2 item 1: every write/end path to a claimed pane is Human-only ----

async fn claimed(fixture: &Fixture) -> Result<String> {
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    anyhow::ensure!(status == 200, "{body}");
    Ok(body["instance"]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned())
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// Open `/v1/follow?tty=1` for `id` as `header`, then send one TTY input
/// frame. Returns the follow socket, or the handshake error.
async fn follow_and_type(
    fixture: &Fixture,
    header: (&'static str, String),
    id: &str,
) -> Result<Ws> {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let mut req = format!("ws://{}/v1/follow?instanceId={id}&tty=1", fixture.hub.addr)
        .into_client_request()?;
    req.headers_mut().insert(header.0, header.1.parse()?);
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(req),
    )
    .await??;
    let uuid =
        remuda_protocol::StreamUuid::from_prefixed_id(remuda_protocol::Id::new("tty")?.as_str())
            .context("stream uuid")?;
    let frame = remuda_protocol::encode_binary_frame(
        remuda_protocol::BinaryChannel::TtyInput,
        uuid,
        0,
        b"rm -rf ~\r",
    )?;
    ws.send(Message::Binary(frame.into())).await?;
    Ok(ws)
}

/// The next `tty.diagnostic` on a follow socket, if one arrives in time.
async fn diagnostic(ws: &mut Ws) -> Option<Value> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(value) = serde_json::from_str::<Value>(&text)
                    && value["type"] == "tty.diagnostic"
                {
                    return Some(value);
                }
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
            _ => return None,
        }
    }
    None
}

#[tokio::test]
async fn follow_socket_tty_input_to_a_claimed_pane_is_human_only() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claimed(&fixture).await?;
    let bot = fixture
        .hub
        .mint_bot_device_token("hclaim-bot-follow")
        .await?;
    let mut ws = follow_and_type(&fixture, ("Authorization", format!("Bearer {bot}")), &id).await?;
    let refused = diagnostic(&mut ws)
        .await
        .context("bot input got no diagnostic")?;
    assert_eq!(refused["reason"], "human-only", "{refused}");
    assert_eq!(refused["operation"], "tty.write");
    assert!(
        fixture.node.params_of("tty.write").is_empty(),
        "a bot keystroke reached the node"
    );

    let agent = fixture
        .hub
        .test_mint_agent_token("hclaim-agent-follow", &id)
        .await?;
    assert!(
        follow_and_type(&fixture, ("Authorization", format!("Bearer {agent}")), &id)
            .await
            .is_err(),
        "an agent cannot open the follow socket at all"
    );
    assert!(fixture.node.params_of("tty.write").is_empty());

    let _human = follow_and_type(&fixture, ("Cookie", fixture.cookie.clone()), &id).await?;
    for _ in 0..50 {
        if !fixture.node.params_of("tty.write").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let writes = fixture.node.params_of("tty.write");
    assert_eq!(writes.len(), 1, "the human keystroke reaches the node");
    assert_eq!(writes[0]["origin"], "human");
    assert!(
        writes[0]["claimWrite"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{}",
        writes[0]
    );
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn broadcast_skips_a_claimed_pane_for_a_non_human() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claimed(&fixture).await?;
    let body = json!({
        "operation": "instance.send",
        "payload": {"input": {"type": "prompt", "text": "hi"}},
        "kinds": ["claude"],
    });
    let bot = fixture
        .hub
        .mint_bot_device_token("hclaim-bot-fleet")
        .await?;
    let (status, refused) = fixture
        .call(
            "POST",
            "/v1/fleet/broadcast",
            &[("Authorization", format!("Bearer {bot}"))],
            Some(&body),
        )
        .await?;
    assert_eq!(status, 200, "{refused}");
    let row = refused["results"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["instanceId"] == json!(id)))
        .context("claimed row in the broadcast results")?;
    assert_eq!(row["ok"], false, "{row}");
    assert_eq!(row["reason"], "human-only", "{row}");
    let mut tty = body.clone();
    tty["operation"] = json!("tty.write");
    tty["payload"] = json!({"keys": ["enter"]});
    let _ = fixture
        .call(
            "POST",
            "/v1/fleet/broadcast",
            &[("Authorization", format!("Bearer {bot}"))],
            Some(&tty),
        )
        .await?;
    let agent = fixture
        .hub
        .test_mint_agent_token("hclaim-agent-fleet", &id)
        .await?;
    let _ = fixture
        .call(
            "POST",
            "/v1/fleet/broadcast",
            &[("Authorization", format!("Bearer {agent}"))],
            Some(&body),
        )
        .await?;
    assert!(
        fixture.node.params_of("instance.send").is_empty(),
        "a non-human broadcast reached the pane"
    );
    assert!(fixture.node.params_of("tty.write").is_empty());

    let (status, ok) = fixture
        .call("POST", "/v1/fleet/broadcast", &fixture.human(), Some(&body))
        .await?;
    assert_eq!(status, 200, "{ok}");
    let sends = fixture.node.params_of("instance.send");
    assert_eq!(sends.len(), 1, "a human broadcast reaches the claimed pane");
    assert_eq!(sends[0]["origin"], "human");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn delete_of_a_claimed_instance_is_human_only() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claimed(&fixture).await?;
    let bot = fixture
        .hub
        .mint_bot_device_token("hclaim-bot-delete")
        .await?;
    let (status, refused) = fixture
        .call(
            "DELETE",
            &format!("/v1/instances/{id}?force=1"),
            &[("Authorization", format!("Bearer {bot}"))],
            None,
        )
        .await?;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["reason"], "human-only", "{refused}");
    let agent = fixture
        .hub
        .test_mint_agent_token("hclaim-agent-delete", &id)
        .await?;
    let (status, _) = fixture
        .call(
            "DELETE",
            &format!("/v1/instances/{id}?force=1"),
            &[("Authorization", format!("Bearer {agent}"))],
            None,
        )
        .await?;
    assert_eq!(status, 403);
    for method in ["instance.close", "instance.purge"] {
        assert!(
            fixture.node.params_of(method).is_empty(),
            "{method} reached the node"
        );
    }
    let (_, row) = fixture
        .call(
            "GET",
            &format!("/v1/instances/{id}"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(row["lifecycle"], "ready", "the claim is still live: {row}");

    let (status, deleted) = fixture
        .call(
            "DELETE",
            &format!("/v1/instances/{id}?force=1"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{deleted}");
    let closes = fixture.node.params_of("instance.close");
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0]["origin"], "human");
    assert!(closes[0]["claimWrite"].as_str().is_some());
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_unstamped_pane_write_is_never_forwarded() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claimed(&fixture).await?;
    // A Hub-internal (or forgotten) path queues a send without the gate.
    let store = fixture.hub.store().context("store")?;
    let (command, _) = store
        .queue_command(
            None,
            Some(id.clone()),
            fixture.host.clone(),
            "instance.send".into(),
            json!({"instanceId": id, "origin": "human", "input": {"type": "prompt", "text": "x"}}),
            None,
            None,
            None,
            None,
        )
        .await?;
    assert!(
        fixture
            .hub
            .test_handle()
            .test_forward_command_online(&command.command_id)
            .await
            .is_err()
    );
    assert!(
        fixture.node.params_of("instance.send").is_empty(),
        "the unstamped send reached the node"
    );
    let row = store
        .get_command(command.command_id.clone())
        .await?
        .context("row")?;
    assert_ne!(
        row.state, "queued",
        "the refused row settles: {}",
        row.state
    );
    fixture.hub.shutdown().await;
    Ok(())
}

// ---- D-062 r2 item 4: a claim the Hub never confirmed is still released ----

#[tokio::test]
async fn a_claim_whose_commit_reply_is_lost_is_detached() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .node
        .drop_claim_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert!(status >= 400, "{status} {body}");
    let claimed = fixture.node.params_of("instance.claim");
    assert_eq!(
        claimed.len(),
        1,
        "the Node received (and may hold) the claim"
    );
    let id = claimed[0]["instanceId"].as_str().context("id")?.to_owned();
    let closes = fixture.node.params_of("instance.close");
    assert_eq!(
        closes.len(),
        1,
        "the detach is queued for the unconfirmed claim"
    );
    assert_eq!(closes[0]["instanceId"], json!(id));
    assert_eq!(closes[0]["origin"], "human");
    let audit = fixture.audit(&id).await?;
    assert!(
        audit.iter().any(|row| row["action"] == "instance.release"
            && row["detail"]["reason"] == "claim-commit-unconfirmed"),
        "{audit:?}"
    );
    // Already released: a later release is a no-op.
    let (status, again) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["released"], false);
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn release_detaches_an_ended_claim_row_that_was_never_released() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claimed(&fixture).await?;
    // The row ended without a release (e.g. failed before this fix).
    fixture
        .hub
        .store()
        .context("store")?
        .fail_instance(id.clone(), "herdr-claim-refused".into())
        .await?;
    let (status, first) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["released"], true, "{first}");
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);
    let (_, second) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(second["released"], false, "{second}");
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);
    fixture.hub.shutdown().await;
    Ok(())
}

/// Poll until `done` holds (bounded).
async fn eventually(what: &str, mut done: impl FnMut() -> bool) -> Result<()> {
    let start = std::time::Instant::now();
    while !done() {
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(15),
            "never happened: {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

/// D-062 r3 item 1: the claim commits on the Node and the link dies before
/// the reply, so the Hub queues the detach with NO transport registered
/// (`kind_of` is None). Returns the instance id.
async fn claim_while_the_link_dies(fixture: &Fixture) -> Result<String> {
    fixture
        .node
        .node_uniqueness
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *fixture.node.vanish_on_claim.lock().unwrap() =
        Some((fixture.hub.test_handle(), fixture.host.clone()));
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert!(status >= 400, "{status} {body}");
    let claimed = fixture.node.params_of("instance.claim");
    assert_eq!(claimed.len(), 1, "the Node received (and holds) the claim");
    let id = claimed[0]["instanceId"].as_str().context("id")?.to_owned();
    assert_eq!(
        fixture
            .node
            .held
            .lock()
            .unwrap()
            .get(&id)
            .map(String::as_str),
        Some("w1:p1")
    );
    assert!(
        fixture.node.params_of("instance.close").is_empty(),
        "nothing can be delivered while the host is away"
    );
    let commands = fixture
        .hub
        .store()
        .context("store")?
        .list_instance_commands(id.clone(), 20)
        .await?;
    let closes: Vec<_> = commands
        .iter()
        .filter(|command| command.operation == "instance.close")
        .collect();
    assert_eq!(closes.len(), 1, "{commands:?}");
    assert_eq!(
        (closes[0].state.as_str(), closes[0].forwarded),
        ("queued", false)
    );
    // A release while the host is away does not consume the detach: it is
    // still owed, so nothing new is queued and the release is not "done".
    let (status, release) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{release}");
    assert_eq!(release["released"], false, "{release}");
    let audit = fixture.audit(&id).await?;
    let release_audit = audit
        .iter()
        .find(|row| row["action"] == "instance.release")
        .context("release audit")?;
    assert_eq!(
        release_audit["detail"]["commandId"],
        json!(closes[0].command_id),
        "the audit names the detach whose state says if it was delivered"
    );
    Ok(id)
}

async fn reclaim_after_detach(fixture: &Fixture, id: &str) -> Result<()> {
    let node = fixture.node.clone();
    eventually("the queued detach reaches the Node", || {
        node.params_of("instance.close").len() == 1
    })
    .await?;
    let closes = fixture.node.params_of("instance.close");
    assert_eq!(closes[0]["instanceId"], json!(id));
    assert_eq!(closes[0]["origin"], "human");
    assert!(fixture.node.held.lock().unwrap().is_empty());
    // Several hello/tick passes later it is still delivered exactly once.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);
    let (status, again) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["released"], false, "delivered: a no-op now");
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);
    // The Node freed the pane, so it can be claimed again.
    let (status, claimed) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert_eq!(status, 200, "{claimed}");
    Ok(())
}

#[tokio::test]
async fn a_detach_queued_while_the_node_is_away_is_delivered_on_its_hello() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claim_while_the_link_dies(&fixture).await?;
    // The Node reconnects.
    fixture
        .hub
        .test_set_node_transport(&fixture.host, fixture.node.clone())
        .await;
    fixture
        .hub
        .test_handle()
        .test_run_post_hello(&fixture.host)
        .await;
    reclaim_after_detach(&fixture, &id).await?;
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_detach_queued_while_the_node_is_away_is_delivered_by_the_tick() -> Result<()> {
    let fixture = Fixture::new().await?;
    let id = claim_while_the_link_dies(&fixture).await?;
    // The transport comes back without this test running the hello path:
    // the Hub tick alone delivers the owed detach.
    fixture
        .hub
        .test_set_node_transport(&fixture.host, fixture.node.clone())
        .await;
    reclaim_after_detach(&fixture, &id).await?;
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_detach_the_node_refused_is_queued_again_by_release() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .node
        .drop_claim_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fixture
        .node
        .refuse_closes
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/instances/claim",
            &fixture.human(),
            Some(&fixture.claim_body("w1:t1", None)),
        )
        .await?;
    assert!(status >= 400, "{status} {body}");
    let id = fixture.node.params_of("instance.claim")[0]["instanceId"]
        .as_str()
        .context("id")?
        .to_owned();
    // The detach reached the Node, which refused it: the claim is still live
    // there, although a release was audited.
    assert_eq!(fixture.node.params_of("instance.close").len(), 1);
    assert!(fixture.node.held.lock().unwrap().contains_key(&id));
    assert!(
        fixture
            .audit(&id)
            .await?
            .iter()
            .any(|row| row["action"] == "instance.release")
    );
    // The audit is not delivery: a release queues the detach again.
    let (status, again) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["released"], true, "{again}");
    assert_eq!(fixture.node.params_of("instance.close").len(), 2);
    assert!(fixture.node.held.lock().unwrap().is_empty());
    let (_, third) = fixture
        .call(
            "POST",
            &format!("/v1/instances/{id}/release"),
            &fixture.human(),
            None,
        )
        .await?;
    assert_eq!(third["released"], false, "{third}");
    assert_eq!(fixture.node.params_of("instance.close").len(), 2);
    fixture.hub.shutdown().await;
    Ok(())
}
