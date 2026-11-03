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

/// Scripted Node answering the claim RPCs the way `remuda-node` does,
/// recording every method and its params.
#[derive(Default)]
struct FakeNode {
    calls: Mutex<Vec<(String, Value)>>,
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
