//! Dispatch must run the driver it recorded, resolve a native-login profile to
//! no delegation, and give back what it provisioned when a later step fails.
//!
//! Every test here pins a defect observed on 2026-09-17 (see
//! `docs/design/evidence/dispatch-driver-1.md`):
//!
//! * a dispatch recorded `claude-pty` while the Node ran `claude-print`;
//! * a `native`-kind provider profile (secret-less by design) was admitted as a
//!   pin and then failed the launch with 400 "provider profile has no stored
//!   auth token";
//! * that failure landed after `worker.provision` and before any roster row, so
//!   `remuda retire` had nothing to reclaim and cleanup was manual.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const BRIEF: &str =
    "Do the tiny task in $WORKTREE.\nReply on one line: DONE <sha> or BLOCKED <reason>.\n";

/// What the fake Node advertises it can launch, and what it does when asked.
#[derive(Clone, Copy)]
struct NodeBehaviour {
    /// `capabilities.driverInventory[].launchable` for `shell-pty`. `None`
    /// reports no inventory at all (an older Node).
    shell_pty_launchable: Option<bool>,
    /// Whether the host advertises a herdr socket.
    herdr: bool,
    /// Driver the Node claims it built, regardless of what was requested. `None`
    /// echoes the request back, which is what an honest Node does.
    pretend_driver: Option<&'static str>,
}

impl Default for NodeBehaviour {
    fn default() -> Self {
        Self {
            shell_pty_launchable: Some(true),
            herdr: true,
            pretend_driver: None,
        }
    }
}

/// One connected fake Node. Records the methods it was asked to run, the driver
/// each `instance.create` requested, and can be told to reject the create.
struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    calls: mpsc::UnboundedReceiver<String>,
    created_drivers: mpsc::UnboundedReceiver<String>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
    }
}

async fn enroll_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace: &str,
    behaviour: NodeBehaviour,
    fail_create: Arc<AtomicBool>,
) -> Result<FakeNode> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    let mut host = json!({
        "hostname": format!("{host_id}.local"),
        "labels": {"toolchain": "rust"},
        "maxInstances": 8,
        "resources": {
            "cpuCount": 8, "cpuPct": 5, "memPct": 20,
            "loadAvg1": 0.4, "diskFreeGb": 120.0,
        },
        "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"logged_in"}],
        "workspaces": [{ "workspaceId": workspace, "hostId": host_id, "root": format!("/tmp/{workspace}") }],
        "workspaceRevision": 1,
    });
    if behaviour.herdr {
        host["herdr"] = json!({"version": "0.9.0", "socket": "/tmp/fake-herdr.sock"});
    }
    // The Node's own report of whether `shell-pty` can launch an *agent* here.
    if let Some(launchable) = behaviour.shell_pty_launchable {
        host["driverInventory"] = json!([{
            "kind": "shell-pty",
            "launchable": launchable,
            "reasonCode": if launchable { "carrier-native" } else { "carrier-not-enabled" },
        }]);
    }
    let capabilities = host
        .get("driverInventory")
        .map(|inventory| json!({ "driverInventory": inventory }));
    let mut params = json!({
        "hostId": host_id,
        "nodeVersion": "0.1.0-test",
        "label": format!("fake-{host_id}"),
        "host": host,
    });
    if let Some(capabilities) = capabilities {
        params["capabilities"] = capabilities;
    }
    node.send(Message::Text(
        json!({"jsonrpc": "2.0", "id": "hello", "method": "runtime.hello", "params": params})
            .to_string()
            .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["nodeToken"].is_string(), "hello {hello}");

    let (tx, calls) = mpsc::unbounded_channel();
    let (driver_tx, created_drivers) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            let Some(id) = frame.get("id") else { continue };
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            let _ = tx.send(method.to_string());
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            let mut error = None;
            let result = match method {
                "worker.provision" => {
                    let name = params["name"].as_str().unwrap_or("x");
                    json!({
                        "name": name,
                        "branch": params["branch"].as_str().unwrap_or("wt/x/work"),
                        "startPoint": "origin/main",
                        "worktreePath": format!("/tmp/remuda-wt/{name}"),
                        "targetDir": format!("/tmp/remuda-target/{name}"),
                    })
                }
                "worker.remove" => json!({
                    "name": params["name"].as_str().unwrap_or("x"),
                    "worktreeRemoved": true,
                    "targetRemoved": true,
                    "reclaimedBytes": "4096",
                }),
                "instance.create" if fail_create.load(Ordering::SeqCst) => {
                    // The real observed failure: the Hub refused the launch (400
                    // "provider profile has no stored auth token") after the
                    // worktree already existed on this Node.
                    error = Some(json!({"code": -32000, "message": "create refused by fixture"}));
                    json!({})
                }
                "instance.create" => {
                    let requested = params
                        .pointer("/spec/driver")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let _ = driver_tx.send(requested.to_string());
                    // A dishonest Node reports a driver other than the one it
                    // was asked for; the Hub must record what it reports.
                    let ran = behaviour.pretend_driver.unwrap_or(requested);
                    // Native-login profiles must reach the Node with no token.
                    if params.pointer("/spec/providerAuthToken").is_some() {
                        let _ = driver_tx.send("LEAKED_TOKEN".into());
                    }
                    json!({
                        "accepted": true,
                        "instanceId": params["instanceId"].as_str().unwrap_or(""),
                        "instance": { "driver": ran },
                    })
                }
                "instance.send" => json!({"accepted": true}),
                "instance.close" => json!({"accepted": true}),
                _ => json!({"ok": true}),
            };
            let response = match error {
                Some(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                None => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            };
            if node
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(FakeNode {
        task,
        calls,
        created_drivers,
    })
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
    node: FakeNode,
    fail_create: Arc<AtomicBool>,
}

impl Ctx {
    async fn spawn(behaviour: NodeBehaviour) -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("dispatch-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let fail_create = Arc::new(AtomicBool::new(false));
        let node =
            enroll_node(&hub, &host, &workspace, behaviour, Arc::clone(&fail_create)).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            node,
            fail_create,
        })
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut builder = self
            .http
            .request(
                method.parse().unwrap(),
                format!("http://{}{}", self.hub.addr, path),
            )
            .bearer_auth(&self.human);
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(json!(null)))
    }

    async fn create_project(&self) -> Result<String> {
        let body = json!({
            "name": "dispatch-driver-test",
            "members": [{ "hostId": self.host, "workspaceId": self.workspace, "role": "build" }],
            "hosts": [{
                "hostId": self.host, "maxInstances": 8, "maxBuilding": 4,
                "diskBudgetGb": 10, "portBlocks": ["58600-58629"],
                "requires": ["toolchain=rust"], "latencyClass": "remote",
            }],
        });
        let (status, body) = self.request("POST", "/v1/projects", Some(body)).await;
        anyhow::ensure!(status == 200, "project {status} {body}");
        Ok(body["id"].as_str().context("project id")?.to_string())
    }

    fn dispatch_body(&self, project: &str) -> Value {
        json!({
            "projectId": project, "brief": BRIEF,
            "briefName": "brief.md", "harness": "claude",
        })
    }

    fn drain_calls(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(method) = self.node.calls.try_recv() {
            out.push(method);
        }
        out
    }

    fn drain_created_drivers(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(driver) = self.node.created_drivers.try_recv() {
            out.push(driver);
        }
        out
    }
}

// ── driver fidelity ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_native_carrier_host_dispatches_on_shell_pty_end_to_end() -> Result<()> {
    // The default this replaces could not pick `shell-pty` at all: it read only
    // `herdr` and answered `claude-pty`, or `claude-print` when herdr was absent.
    let mut ctx = Ctx::spawn(NodeBehaviour::default()).await?;
    let project = ctx.create_project().await?;
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(&project)),
        )
        .await;
    anyhow::ensure!(status == 200, "dispatch {status} {body}");
    // Recorded on the roster …
    assert_eq!(body["worker"]["driver"], "shell-pty", "{body}");
    // … and actually requested of the Node.
    let requested = ctx.drain_created_drivers();
    assert_eq!(requested, vec!["shell-pty".to_string()], "{requested:?}");
    // … and readable back through the roster row.
    let name = body["worker"]["name"].as_str().context("name")?;
    let (status, row) = ctx
        .request("GET", &format!("/v1/workers/{name}"), None)
        .await;
    anyhow::ensure!(status == 200, "get {status} {row}");
    assert_eq!(row["driver"], "shell-pty", "{row}");
    Ok(())
}

#[tokio::test]
async fn a_herdr_only_host_dispatches_on_claude_pty_never_print() -> Result<()> {
    let mut ctx = Ctx::spawn(NodeBehaviour {
        shell_pty_launchable: Some(false),
        herdr: true,
        pretend_driver: None,
    })
    .await?;
    let project = ctx.create_project().await?;
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(&project)),
        )
        .await;
    anyhow::ensure!(status == 200, "dispatch {status} {body}");
    assert_eq!(body["worker"]["driver"], "claude-pty", "{body}");
    assert_eq!(ctx.drain_created_drivers(), vec!["claude-pty".to_string()]);
    Ok(())
}

#[tokio::test]
async fn a_host_with_neither_carrier_is_refused_rather_than_run_on_print() -> Result<()> {
    // `claude-print` exits after one turn, so a worker on it can never be
    // nudged, steered or watched — it must never be reached by default (D-028).
    let mut ctx = Ctx::spawn(NodeBehaviour {
        shell_pty_launchable: Some(false),
        herdr: false,
        pretend_driver: None,
    })
    .await?;
    let project = ctx.create_project().await?;
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(&project)),
        )
        .await;
    anyhow::ensure!(status >= 400, "expected refusal, got {status} {body}");
    let rendered = body.to_string();
    assert!(rendered.contains("REMUDA_PTY_CARRIER"), "{rendered}");
    // Nothing was created, and nothing provisioned was left behind.
    assert!(ctx.drain_created_drivers().is_empty());
    let calls = ctx.drain_calls();
    assert!(!calls.contains(&"instance.create".to_string()), "{calls:?}");
    Ok(())
}

#[tokio::test]
async fn an_explicit_driver_the_host_cannot_launch_is_refused_not_replaced() -> Result<()> {
    let mut ctx = Ctx::spawn(NodeBehaviour {
        shell_pty_launchable: Some(false),
        herdr: true,
        pretend_driver: None,
    })
    .await?;
    let project = ctx.create_project().await?;
    let mut body = ctx.dispatch_body(&project);
    body["driver"] = json!("shell-pty");
    let (status, response) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    // 409: an operator instruction that cannot be honoured is refused with a
    // reason, so the CLI exits non-zero rather than running something else.
    assert_eq!(status, 409, "{response}");
    assert!(ctx.drain_created_drivers().is_empty());
    Ok(())
}

#[tokio::test]
async fn the_roster_records_the_driver_the_node_reports_not_the_one_requested() -> Result<()> {
    // A Node that still downgrades must not be able to leave the Hub claiming
    // the requested carrier. This is the exact divergence that was observed.
    let mut ctx = Ctx::spawn(NodeBehaviour {
        shell_pty_launchable: Some(true),
        herdr: true,
        pretend_driver: Some("claude-print"),
    })
    .await?;
    let project = ctx.create_project().await?;
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(&project)),
        )
        .await;
    anyhow::ensure!(status == 200, "dispatch {status} {body}");
    assert_eq!(ctx.drain_created_drivers(), vec!["shell-pty".to_string()]);
    assert_eq!(
        body["worker"]["driver"], "claude-print",
        "the roster must name what actually ran: {body}"
    );
    Ok(())
}

// ── native-login provider profile ──────────────────────────────────────────

#[tokio::test]
async fn a_native_profile_pin_launches_with_no_delegation_and_no_token() -> Result<()> {
    // A `native` profile is a named account row the Hub holds no token for: the
    // CLI on the host uses its own login. Classifying it as `gateway` made
    // dispatch demand a secret that cannot exist, failing with 400 "provider
    // profile has no stored auth token".
    let mut ctx = Ctx::spawn(NodeBehaviour::default()).await?;
    let project = ctx.create_project().await?;
    let (status, profile) = ctx
        .request(
            "POST",
            "/v1/providers",
            Some(json!({
                "name": "host-native-login",
                "kind": "native",
                "models": ["claude-opus-5"],
                "scope": format!("host:{}", ctx.host),
            })),
        )
        .await;
    anyhow::ensure!(status == 200, "provider {status} {profile}");
    let profile_id = profile["id"].as_str().context("profile id")?.to_string();
    // Secret-less by design — this is what the launch path must accommodate.
    assert_eq!(profile["secret"]["present"], false, "{profile}");

    let mut body = ctx.dispatch_body(&project);
    body["providerProfileId"] = json!(profile_id);
    let (status, response) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    anyhow::ensure!(
        status == 200,
        "a native profile must not demand a launch secret: {status} {response}"
    );
    let rendered = response.to_string();
    assert!(!rendered.contains("no stored auth token"), "{rendered}");
    // No providerAuthToken reached the Node's params.
    let seen = ctx.drain_created_drivers();
    assert!(
        !seen.iter().any(|entry| entry == "LEAKED_TOKEN"),
        "a native launch must carry no providerAuthToken: {seen:?}"
    );
    Ok(())
}

// ── rollback on a failed dispatch ──────────────────────────────────────────

#[tokio::test]
async fn a_failure_after_provisioning_gives_back_the_worktree() -> Result<()> {
    // The leak this closes: the failure landed after `worker.provision` and
    // before any roster row, so `remuda retire` had nothing to reclaim and the
    // worktree and target dir had to be removed by hand.
    let mut ctx = Ctx::spawn(NodeBehaviour::default()).await?;
    let project = ctx.create_project().await?;
    ctx.fail_create.store(true, Ordering::SeqCst);
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(&project)),
        )
        .await;
    anyhow::ensure!(status >= 400, "expected failure, got {status} {body}");
    let calls = ctx.drain_calls();
    assert!(
        calls.contains(&"worker.provision".to_string()),
        "the fixture must have provisioned first: {calls:?}"
    );
    assert!(
        calls.contains(&"worker.remove".to_string()),
        "dispatch must give back what it provisioned: {calls:?}"
    );
    // And no half-built roster row is left behind.
    let (status, list) = ctx
        .request("GET", &format!("/v1/workers?project={project}"), None)
        .await;
    anyhow::ensure!(status == 200, "list {status} {list}");
    assert_eq!(list["items"].as_array().map(Vec::len), Some(0), "{list}");
    Ok(())
}
