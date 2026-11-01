//! ma-admission (D-057 §4.2 items 1-6): Agent-origin dispatch and worker
//! routes, "never beyond the creator" (OA1) on the Hub side, the creator
//! stamp and its commit-point re-read, the D-017 approval decided before any
//! allocation, the scoped worker respawn helper, and the D-051(b) amendment.
//!
//! Everything runs against fake Nodes over the real WSS link; no sleeps or
//! millisecond thresholds decide an assertion. The Node half of OA1 is in
//! `crates/remuda-driver/tests/materializer.rs` (`the_creator_rule_matrix_on_the_node`).

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use remuda_protocol::creator_permission::{self, CREATOR_PERMISSION_MODE_FIELD};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// ── fake Node ──────────────────────────────────────────────────────────────

/// What a fake Node advertises in hello.
#[derive(Clone, Copy)]
struct NodeShape {
    /// `capabilities.driverInventory` reports a launchable `shell-pty`.
    native: bool,
    /// The host record advertises herdr.
    herdr: bool,
}

/// A `worker.provision` hold: the Node tells the test it has the call, then
/// replies only after the test releases it.
#[derive(Clone)]
struct ProvisionHold {
    arrived: mpsc::UnboundedSender<Value>,
    release: Arc<Notify>,
}

struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    /// Every Hub→Node call, in order: method and params.
    calls: mpsc::UnboundedReceiver<(String, Value)>,
    #[allow(dead_code)] // the commit-point and approval tests use it
    hold: Arc<Mutex<Option<ProvisionHold>>>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeNode {
    fn drain(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(call) = self.calls.try_recv() {
            out.push(call);
        }
        out
    }

    fn methods(&mut self) -> Vec<String> {
        self.drain().into_iter().map(|(method, _)| method).collect()
    }

    /// Park the next `worker.provision` until the returned notify fires.
    #[allow(dead_code)] // the commit-point test uses it
    fn hold_provision(&self) -> (mpsc::UnboundedReceiver<Value>, Arc<Notify>) {
        let (arrived, arrivals) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        *self.hold.lock().unwrap() = Some(ProvisionHold {
            arrived,
            release: Arc::clone(&release),
        });
        (arrivals, release)
    }
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match tokio::time::timeout(Duration::from_secs(30), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
    }
}

async fn enroll_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    workspace: &str,
    shape: NodeShape,
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
        "maxInstances": 10_000,
        "resources": {"cpuCount": 64, "cpuPct": 1, "memPct": 1, "loadAvg1": 0.1, "diskFreeGb": 500.0},
        "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
        "workspaces": [{ "workspaceId": workspace, "hostId": host_id, "root": format!("/srv/{workspace}") }],
        "workspaceRevision": 1,
    });
    if shape.herdr {
        host["herdr"] = json!({"version": "0.9.0", "socket": "/run/fake-herdr.sock"});
    }
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": format!("fake-{host_id}"),
                "capabilities": {
                    "lineageFence": true,
                    "driverInventory": [{
                        "kind": "shell-pty",
                        "launchable": shape.native,
                        "reasonCode": if shape.native { "carrier-native" } else { "carrier-not-enabled" },
                    }],
                },
                "host": host,
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["nodeToken"].is_string(), "hello {hello}");
    let (tx, rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let hold: Arc<Mutex<Option<ProvisionHold>>> = Arc::new(Mutex::new(None));
    let task_hold = Arc::clone(&hold);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(reply) = out_rx.recv() => {
                    if node.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                }
                frame = recv_json(&mut node) => {
                    let Ok(frame) = frame else { break };
                    let (Some(id), Some(method)) = (
                        frame.get("id").cloned(),
                        frame.get("method").and_then(Value::as_str).map(str::to_owned),
                    ) else {
                        continue;
                    };
                    let params = frame.get("params").cloned().unwrap_or(json!({}));
                    let _ = tx.send((method.clone(), params.clone()));
                    let result = match method.as_str() {
                        "worker.provision" => {
                            let name = params["name"].as_str().unwrap_or("x").to_string();
                            let branch = params["branch"].as_str().unwrap_or("wt/x/work").to_string();
                            let result = json!({
                                "name": name,
                                "branch": branch,
                                "startPoint": "origin/main",
                                "worktreePath": format!("/srv/wt/{name}"),
                                "targetDir": format!("/srv/target/{name}"),
                            });
                            if let Some(hold) = task_hold.lock().unwrap().take() {
                                let out = out_tx.clone();
                                tokio::spawn(async move {
                                    let _ = hold.arrived.send(params);
                                    hold.release.notified().await;
                                    let _ = out.send(json!({"jsonrpc":"2.0","id":id,"result":result}));
                                });
                                continue;
                            }
                            result
                        }
                        "worker.remove" => json!({
                            "name": params["name"].as_str().unwrap_or("x"),
                            "worktreeRemoved": true,
                            "targetRemoved": true,
                            "reclaimedBytes": "0",
                        }),
                        "lineage.fence" => json!({
                            "fenceId": params["fenceId"], "closedInstanceIds": [], "outcomes": [],
                        }),
                        "tty.screen" => json!({"supported": false}),
                        _ => json!({"accepted": true}),
                    };
                    let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    if node.send(Message::Text(response.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(FakeNode {
        task,
        calls: rx,
        hold,
    })
}

// ── harness ────────────────────────────────────────────────────────────────

const BRIEF: &str = "Do the tiny task.\nReply on one line: DONE <sha> or BLOCKED <reason>.\n";

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    /// The seat's host.
    host: String,
    workspace: String,
    node: FakeNode,
    /// A second project member host, for cross-host dispatch.
    far_host: String,
    far_workspace: String,
    #[allow(dead_code)] // the cross-host approval test uses it
    far_node: FakeNode,
    project: String,
}

struct Reply {
    status: u16,
    body: Value,
}

impl Ctx {
    /// Hub with two member hosts. The seat's host carries herdr and the
    /// launchable native carrier, so both the shell-driver and the
    /// approval-free herdr carrier are reachable on it.
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("admission-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let far_host = HostId::new().as_id().to_string();
        let far_workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let shape = NodeShape {
            native: true,
            herdr: true,
        };
        let node = enroll_node(&hub, &host, &workspace, shape).await?;
        let far_node = enroll_node(&hub, &far_host, &far_workspace, shape).await?;
        let mut ctx = Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            node,
            far_host,
            far_workspace,
            far_node,
            project: String::new(),
        };
        ctx.wait_online().await?;
        let project = ctx
            .request(
                "POST",
                "/v1/projects",
                Some(json!({
                    "name": "admission",
                    "members": [
                        { "hostId": ctx.host, "workspaceId": ctx.workspace, "role": "build" },
                        { "hostId": ctx.far_host, "workspaceId": ctx.far_workspace, "role": "build" },
                    ],
                    "policy": { "configurable": {
                        "coordinatorFanOut": 1000,
                        "allowMultipleDispatchers": true,
                        "nudgeThrottleMins": 0,
                    }},
                })),
            )
            .await;
        anyhow::ensure!(project.status == 200, "project {}", project.body);
        ctx.project = project.body["id"].as_str().unwrap().to_string();
        Ok(ctx)
    }

    /// Wait until both Nodes read online (event-driven poll of the Hub's own
    /// host list; the bound is a failure deadline, not a timing assertion).
    async fn wait_online(&self) -> Result<()> {
        for _ in 0..400 {
            let hosts = self.request("GET", "/v1/hosts", None).await;
            let online = hosts.body["items"]
                .as_array()
                .or_else(|| hosts.body.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter(|host| host["online"].as_bool() == Some(true))
                        .count()
                })
                .unwrap_or(0);
            if online >= 2 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("fake Nodes never came online")
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.send(method, path, body, &self.human, &[]).await
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut builder = self
            .http
            .request(
                method.parse().unwrap(),
                format!("http://{}{}", self.hub.addr, path),
            )
            .bearer_auth(token);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.unwrap();
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(json!(null));
        Reply { status, body }
    }

    fn store(&self) -> &remuda_hub::store_test_support::Store {
        self.hub.store().unwrap()
    }

    /// Seat an Agent: a claude-sdk chapter on the seat host holding
    /// `dispatch`, scoped to the project, running `mode` (as `kind`).
    async fn seat(&self, kind: &str, mode: &str) -> Seat {
        let project = remuda_protocol::ProjectId::try_from(self.project.clone()).unwrap();
        let delegation = remuda_hub::store_test_support::seat_delegation(
            remuda_protocol::InstanceScope {
                project_ids: vec![project],
                ..Default::default()
            },
            vec!["dispatch".into()],
        );
        let instance = self
            .store()
            .insert_instance_delegated(
                self.host.clone(),
                Some(self.workspace.clone()),
                kind.into(),
                "claude-sdk".into(),
                Some("seat".into()),
                json!({ "kind": kind, "projectId": self.project, "permissionMode": mode }),
                delegation,
                remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
            )
            .await
            .expect("seat insert");
        let token = remuda_hub::instance_token(self.store().clone(), instance.instance_id.clone())
            .await
            .expect("seat token");
        Seat {
            id: instance.instance_id,
            token,
        }
    }

    /// The `instance.create` / `instance.resume` frames a Node received.
    fn creates(calls: &[(String, Value)]) -> Vec<Value> {
        calls
            .iter()
            .filter(|(method, _)| method == "instance.create" || method == "instance.resume")
            .map(|(_, params)| params.clone())
            .collect()
    }
}

struct Seat {
    id: String,
    token: String,
}

fn reason(reply: &Reply) -> &str {
    reply.body["reason"].as_str().unwrap_or_default()
}

// ── item 3: never beyond the creator, Hub side ───────────────────────────

/// Every OA1 cell on the Hub side: child harness × creator mode × requested
/// mode (every word of the child's vocabulary, a foreign word, and omitted),
/// through the create paths' own admission (`restrict_permission`) against
/// seats whose recorded modes the Hub reads back from the store.
#[tokio::test]
async fn the_creator_rule_matrix_on_the_hub() {
    let ctx = Ctx::spawn().await.unwrap();
    let handle = ctx.hub.test_handle();
    let mut cells = 0;
    for creator_kind in ["claude", "codex", "grok", "agy"] {
        for creator_mode in creator_permission::vocabulary(creator_kind) {
            let seat = ctx.seat(creator_kind, creator_mode).await;
            for child in ["claude", "codex", "grok", "agy"] {
                let mut requests: Vec<Option<&str>> = creator_permission::vocabulary(child)
                    .iter()
                    .map(|mode| Some(*mode))
                    .collect();
                requests.push(Some("bypass"));
                requests.push(None);
                for requested in requests {
                    cells += 1;
                    let label = format!("{creator_kind}/{creator_mode} -> {child} {requested:?}");
                    let spec = json!({
                        "kind": child,
                        "permissionMode": requested,
                        CREATOR_PERMISSION_MODE_FIELD: "bypassPermissions",
                    });
                    let outcome = handle
                        .test_restrict_permission(&seat.id, requested, spec)
                        .await;
                    match creator_permission::admit(child, requested, creator_mode) {
                        Ok(effective) => {
                            let spec = outcome.unwrap_or_else(|e| panic!("{label}: {e:?}"));
                            assert_eq!(spec["permissionMode"], effective, "{label}");
                            assert_eq!(spec["permissionModeRequested"], json!(requested));
                            // The stamp is the creator's recorded mode, never
                            // the body's.
                            assert_eq!(spec[CREATOR_PERMISSION_MODE_FIELD], *creator_mode);
                        }
                        Err(refusal) => {
                            let (status, body) = outcome.expect_err(&label);
                            assert_eq!(status, 403, "{label}: {body}");
                            assert_eq!(body["reason"], refusal.reason(), "{label}");
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cells, 16 * 24);
}

/// The same rule end to end through `POST /v1/instances`, one admitted and
/// one refused cell of each kind per child harness: an admitted create
/// reaches the Node with the effective mode, the requested word recorded
/// verbatim and the creator stamp on the top-level params; a refused one
/// reaches no Node at all.
#[tokio::test]
async fn agent_creates_follow_the_creator_rule_end_to_end() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let auto = ctx.seat("claude", "auto").await;
    let off = ctx.seat("claude", "bypassPermissions").await;
    ctx.node.drain();
    for (seat, child, requested, expected) in [
        // Omitted: inherit the creator's own word.
        (&auto, "claude", None, Ok("auto")),
        (&auto, "grok", None, Ok("auto")),
        (&auto, "codex", None, Err("permission-mode-not-inheritable")),
        // Control on is always allowed; control off only under a
        // control-off creator.
        (&auto, "codex", Some("on-request"), Ok("on-request")),
        (
            &auto,
            "claude",
            Some("bypassPermissions"),
            Err("beyond-creator"),
        ),
        (&auto, "agy", Some("always-proceed"), Err("beyond-creator")),
        (&off, "claude", None, Ok("bypassPermissions")),
        (&off, "codex", Some("never"), Ok("never")),
        (&off, "grok", Some("always-approve"), Ok("always-approve")),
        // Only the harness's own words.
        (
            &auto,
            "claude",
            Some("bypass"),
            Err("permission-mode-unknown"),
        ),
    ] {
        let mut body = json!({
            "hostId": ctx.host,
            "workspaceId": ctx.workspace,
            "kind": child,
            "driver": "claude-pty",
            "prompt": "hi",
        });
        if let Some(mode) = requested {
            body["permissionMode"] = json!(mode);
        }
        let reply = ctx
            .send("POST", "/v1/instances", Some(body), &seat.token, &[])
            .await;
        let label = format!("{child} {requested:?}");
        let forwarded = Ctx::creates(&ctx.node.drain());
        match expected {
            Ok(effective) => {
                assert_eq!(reply.status, 200, "{label}: {}", reply.body);
                assert_eq!(forwarded.len(), 1, "{label}");
                let params = &forwarded[0];
                assert_eq!(params["spec"]["permissionMode"], effective, "{label}");
                assert_eq!(params["spec"]["permissionModeRequested"], json!(requested));
                let creator = if std::ptr::eq(seat, &off) {
                    "bypassPermissions"
                } else {
                    "auto"
                };
                assert_eq!(params[CREATOR_PERMISSION_MODE_FIELD], creator, "{label}");
                assert!(
                    params["spec"].get(CREATOR_PERMISSION_MODE_FIELD).is_none(),
                    "{label}: the stamp rides the top level only"
                );
            }
            Err(expected_reason) => {
                assert_eq!(reply.status, 403, "{label}: {}", reply.body);
                assert_eq!(reason(&reply), expected_reason, "{label}");
                assert!(
                    forwarded.is_empty(),
                    "{label}: refused before any Node call"
                );
            }
        }
    }
}

/// A `creatorPermissionMode` in a request body is never the Hub's: it is
/// stripped at the top level and inside the spec, for every origin, and only
/// the creator's recorded mode is stamped.
#[tokio::test]
async fn a_body_supplied_creator_stamp_is_stripped() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let seat = ctx.seat("claude", "auto").await;
    ctx.node.drain();

    // Agent fleet create: the spec is the caller's own JSON. A forged
    // control-off stamp does not turn the creator into a control-off one.
    let forged = json!({
        "hosts": [ctx.host],
        "kind": "claude",
        "driver": "claude-pty",
        "spec": {
            "permissionMode": "bypassPermissions",
            "creatorPermissionMode": "bypassPermissions",
            "workspaceId": ctx.workspace,
        },
    });
    let reply = ctx
        .send(
            "POST",
            "/v1/fleet/instances",
            Some(forged),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert_eq!(reason(&reply), "beyond-creator");
    assert!(Ctx::creates(&ctx.node.drain()).is_empty());

    // An admitted Agent create carries the creator's own mode, whatever the
    // body claimed at either level.
    let reply = ctx
        .send(
            "POST",
            "/v1/fleet/instances",
            Some(json!({
                "hosts": [ctx.host],
                "kind": "claude",
                "driver": "claude-pty",
                "spec": {
                    "permissionMode": "acceptEdits",
                    "creatorPermissionMode": "bypassPermissions",
                    "workspaceId": ctx.workspace,
                },
            })),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let forwarded = Ctx::creates(&ctx.node.drain());
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0][CREATOR_PERMISSION_MODE_FIELD], "auto");
    assert!(
        forwarded[0]["spec"]
            .get(CREATOR_PERMISSION_MODE_FIELD)
            .is_none()
    );

    // A Human create never carries a stamp at all, even a forged one.
    let reply = ctx
        .request(
            "POST",
            "/v1/instances",
            Some(json!({
                "hostId": ctx.host,
                "kind": "claude",
                "driver": "claude-pty",
                "permissionMode": "bypassPermissions",
                "creatorPermissionMode": "bypassPermissions",
                "prompt": "hi",
            })),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let forwarded = Ctx::creates(&ctx.node.drain());
    assert_eq!(forwarded.len(), 1);
    assert!(forwarded[0].get(CREATOR_PERMISSION_MODE_FIELD).is_none());
    assert_eq!(forwarded[0]["spec"]["permissionMode"], "bypassPermissions");
}

/// Bot origin is unchanged: no creator rule, no stamp, an omitted mode is
/// `manual` and every other word passes to the Node (which refuses
/// control-off for Bot, D-011).
#[tokio::test]
async fn bot_origin_creates_are_unchanged() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let bot = ctx
        .hub
        .mint_bot_device_token("admission-bot")
        .await
        .unwrap();
    ctx.node.drain();
    for (requested, forwarded_mode) in [
        (None, "manual"),
        (Some("plan"), "plan"),
        (Some("bypassPermissions"), "bypassPermissions"),
    ] {
        let mut body = json!({
            "hostId": ctx.host, "kind": "claude", "driver": "claude-pty", "prompt": "hi",
        });
        if let Some(mode) = requested {
            body["permissionMode"] = json!(mode);
        }
        let reply = ctx
            .send("POST", "/v1/instances", Some(body), &bot, &[])
            .await;
        assert_eq!(reply.status, 200, "{requested:?}: {}", reply.body);
        let forwarded = Ctx::creates(&ctx.node.drain());
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0]["spec"]["permissionMode"], forwarded_mode);
        assert_eq!(forwarded[0]["origin"], "bot");
        assert!(forwarded[0].get(CREATOR_PERMISSION_MODE_FIELD).is_none());
        assert!(
            forwarded[0]["spec"]
                .get("permissionModeRequested")
                .is_none()
        );
    }
}

/// The commit-point re-read, at the store: a create stamped with a mode the
/// creator no longer runs aborts inside the insert's writer job, and nothing
/// is inserted.
#[tokio::test]
async fn a_stale_creator_stamp_aborts_the_insert() {
    let ctx = Ctx::spawn().await.unwrap();
    let seat = ctx.seat("claude", "auto").await;
    let store = ctx.store();
    let initiator = remuda_hub::store_test_support::initiator_of(store, &seat.id)
        .await
        .expect("seat initiator");
    let insert = |stamp: &str| {
        let spec = json!({
            "kind": "claude", "permissionMode": "auto", "parentInstanceId": seat.id,
            CREATOR_PERMISSION_MODE_FIELD: stamp, "origin": "agent",
        });
        store.insert_instance_delegated(
            ctx.host.clone(),
            Some(ctx.workspace.clone()),
            "claude".into(),
            "claude-pty".into(),
            None,
            spec,
            Default::default(),
            remuda_hub::agent_scope_test_support::CallerAuthority {
                initiator: Some(initiator.clone()),
                device_id: None,
            },
        )
    };
    let before = store.list_instances(None).await.unwrap().len();
    match insert("bypassPermissions").await {
        Err(remuda_hub::store_test_support::StoreError::CreatorModeChanged { stamped, now }) => {
            assert_eq!(
                (stamped.as_str(), now.as_str()),
                ("bypassPermissions", "auto")
            );
        }
        other => panic!("expected CreatorModeChanged, got {other:?}"),
    }
    assert_eq!(store.list_instances(None).await.unwrap().len(), before);
    // The same create under the mode the creator runs commits.
    insert("auto").await.expect("matching stamp commits");
    // A stamp with no Agent initiator is refused, never trusted.
    let orphan = store
        .insert_instance_delegated(
            ctx.host.clone(),
            None,
            "claude".into(),
            "claude-pty".into(),
            None,
            json!({ CREATOR_PERMISSION_MODE_FIELD: "auto" }),
            Default::default(),
            remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
        )
        .await;
    assert!(orphan.is_err(), "{orphan:?}");
}

// ── item 2: dispatch takes a permission mode ─────────────────────────────

impl Ctx {
    /// A same-host herdr dispatch (the approval-free carrier). Named
    /// explicitly: the Hub's own `w<hex>` name allocator draws from the id's
    /// timestamp prefix, so two unnamed dispatches in one project collide.
    fn dispatch_body(&self, name: &str) -> Value {
        json!({
            "projectId": self.project,
            "brief": BRIEF,
            "briefName": "brief.md",
            "harness": "claude",
            "hostId": self.host,
            "carrier": "herdr",
            "name": name,
        })
    }
}

/// A Human dispatch that names no mode is what it was before D-057: the
/// worker launches `bypassPermissions`, the spec carries no requested word
/// and no creator stamp. A named mode is honoured verbatim and recorded.
#[tokio::test]
async fn human_dispatch_keeps_its_default_and_records_a_named_mode() {
    let mut ctx = Ctx::spawn().await.unwrap();
    ctx.node.drain();
    let reply = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("hdefault")),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let forwarded = Ctx::creates(&ctx.node.drain());
    assert_eq!(forwarded.len(), 1);
    let spec = &forwarded[0]["spec"];
    assert_eq!(spec["permissionMode"], "bypassPermissions");
    assert!(spec.get("permissionModeRequested").is_none(), "{spec}");
    assert!(forwarded[0].get(CREATOR_PERMISSION_MODE_FIELD).is_none());
    assert_eq!(forwarded[0]["origin"], "human");
    let worker = &reply.body["worker"];
    assert_eq!(worker["permissionMode"], "bypassPermissions");
    assert!(worker.get("permissionModeRequested").is_none(), "{worker}");

    let mut body = ctx.dispatch_body("hnamed");
    body["permissionMode"] = json!("acceptEdits");
    let reply = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let forwarded = Ctx::creates(&ctx.node.drain());
    assert_eq!(forwarded[0]["spec"]["permissionMode"], "acceptEdits");
    let worker = &reply.body["worker"];
    assert_eq!(worker["permissionModeRequested"], "acceptEdits");
    assert_eq!(worker["permissionMode"], "acceptEdits");
}

/// Bot origin is byte-identical: an omitted mode keeps the dispatch default,
/// no creator rule applies at the Hub, and nothing is stamped.
#[tokio::test]
async fn bot_dispatch_is_unchanged() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let bot = ctx
        .hub
        .mint_bot_device_token("admission-bot")
        .await
        .unwrap();
    ctx.node.drain();
    let reply = ctx
        .send(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("bot1")),
            &bot,
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let forwarded = Ctx::creates(&ctx.node.drain());
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0]["origin"], "bot");
    assert_eq!(forwarded[0]["spec"]["permissionMode"], "bypassPermissions");
    assert!(
        forwarded[0]["spec"]
            .get("permissionModeRequested")
            .is_none()
    );
    assert!(forwarded[0].get(CREATOR_PERMISSION_MODE_FIELD).is_none());
}

// ── item 1: worker routes for Agent origin ────────────────────────────────

impl Ctx {
    /// Approve a pending D-017 one-shot approval as the human owner.
    async fn approve(&self, interaction_id: &str) {
        let pending = self.request("GET", "/v1/interactions", None).await;
        let card = pending.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| {
                item["id"] == json!(interaction_id)
                    || item["interactionId"] == json!(interaction_id)
            })
            .cloned()
            .unwrap_or_else(|| panic!("no card {interaction_id}: {}", pending.body));
        let digest = card
            .pointer("/request/inputDigest")
            .or_else(|| card.pointer("/interaction/request/inputDigest"))
            .and_then(Value::as_str)
            .expect("digest")
            .to_owned();
        let reply = self
            .request(
                "POST",
                &format!("/v1/interactions/{interaction_id}/answer"),
                Some(json!({"answer": {"kind": "approval", "optionId": "allow-once", "inputDigest": digest}})),
            )
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
    }

    /// Pending approval cards the human owner sees.
    async fn pending_approvals(&self) -> usize {
        let pending = self.request("GET", "/v1/interactions", None).await;
        pending.body["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item["kind"] == "approval")
                    .count()
            })
            .unwrap_or(0)
    }
}

/// A seat dispatches a same-host herdr worker with no Remuda approval; with no
/// mode named the worker inherits the seat's own mode, and both values are
/// recorded verbatim. Worker reads are scope-wide.
#[tokio::test]
async fn an_agent_dispatches_and_reads_workers() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let seat = ctx.seat("claude", "auto").await;
    // A worker the owner dispatched by hand in the same project.
    let human = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("byhand")),
        )
        .await;
    assert_eq!(human.status, 200, "{}", human.body);
    ctx.node.drain();

    let reply = ctx
        .send(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("seatw")),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let worker = &reply.body["worker"];
    assert_eq!(worker["permissionMode"], "auto");
    assert!(worker.get("permissionModeRequested").is_none(), "{worker}");
    assert_eq!(worker["driver"], "claude-pty");
    let forwarded = Ctx::creates(&ctx.node.drain());
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0]["origin"], "agent");
    assert_eq!(forwarded[0]["spec"]["parentInstanceId"], seat.id.as_str());
    assert_eq!(forwarded[0]["spec"]["permissionMode"], "auto");
    assert_eq!(forwarded[0]["spec"]["permissionModeRequested"], Value::Null);
    assert_eq!(forwarded[0][CREATOR_PERMISSION_MODE_FIELD], "auto");
    assert_eq!(ctx.pending_approvals().await, 0, "no Remuda prompt");

    // Reads stay scope-wide: the owner's worker is listed and readable too.
    let list = ctx.send("GET", "/v1/workers", None, &seat.token, &[]).await;
    assert_eq!(list.status, 200, "{}", list.body);
    let names: Vec<_> = list.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"byhand".to_string()) && names.contains(&"seatw".to_string()));
    let get = ctx
        .send("GET", "/v1/workers/byhand", None, &seat.token, &[])
        .await;
    assert_eq!(get.status, 200, "{}", get.body);
    let observe = ctx
        .send(
            "POST",
            "/v1/workers/observe",
            Some(json!({})),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(observe.status, 200, "{}", observe.body);
}

/// Every mutating worker verb refuses an Agent on a worker outside its
/// lineage subtree (here: one the owner dispatched by hand) before anything
/// reaches a Node, and admits it on the seat's own worker.
#[tokio::test]
async fn worker_mutations_need_the_worker_in_the_callers_subtree() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let seat = ctx.seat("claude", "auto").await;
    let human = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("byhand")),
        )
        .await;
    assert_eq!(human.status, 200, "{}", human.body);
    let own = ctx
        .send(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("mine")),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(own.status, 200, "{}", own.body);
    ctx.node.drain();
    let verbs = [
        ("nudge", json!({"text": "go on"})),
        ("answer", json!({"key": "enter"})),
        ("switch-model", json!({"model": "claude-opus-5-5"})),
        ("resume", json!({})),
        ("replace", json!({})),
        ("stop", json!({})),
        ("retire", json!({"force": true})),
        ("state", json!({"state": "blocked", "reason": "x"})),
        ("brief", json!({"content": "more"})),
    ];
    for (verb, body) in &verbs {
        let reply = ctx
            .send(
                "POST",
                &format!("/v1/workers/byhand/{verb}"),
                Some(body.clone()),
                &seat.token,
                &[],
            )
            .await;
        assert_eq!(reply.status, 403, "{verb}: {}", reply.body);
        assert_eq!(reason(&reply), "not-owner", "{verb}");
        assert!(
            ctx.node.drain().is_empty(),
            "{verb}: nothing reached the Node"
        );
    }
    // The seat's own worker: the plain mutations are admitted.
    for (verb, body) in [
        ("nudge", json!({"text": "go on"})),
        ("state", json!({"state": "blocked", "reason": "x"})),
        ("brief", json!({"content": "more"})),
        ("stop", json!({})),
        ("retire", json!({"force": true})),
    ] {
        let reply = ctx
            .send(
                "POST",
                &format!("/v1/workers/mine/{verb}"),
                Some(body),
                &seat.token,
                &[],
            )
            .await;
        assert_eq!(reply.status, 200, "{verb}: {}", reply.body);
    }
}

/// Ownership reaches the whole lineage subtree: a worker dispatched by the
/// seat's own sub-coordinator is the seat's to stop, and a sibling seat's
/// worker is not.
#[tokio::test]
async fn the_subtree_reaches_grandchildren_and_stops_at_siblings() {
    let ctx = Ctx::spawn().await.unwrap();
    let seat = ctx.seat("claude", "auto").await;
    let sibling = ctx.seat("claude", "auto").await;
    // A sub-coordinator the seat created (a child holding dispatch).
    let project = remuda_protocol::ProjectId::try_from(ctx.project.clone()).unwrap();
    let sub = ctx
        .store()
        .insert_instance_delegated(
            ctx.host.clone(),
            Some(ctx.workspace.clone()),
            "claude".into(),
            "claude-pty".into(),
            None,
            json!({"kind": "claude", "permissionMode": "auto", "parentInstanceId": seat.id}),
            remuda_hub::store_test_support::seat_delegation(
                remuda_protocol::InstanceScope {
                    project_ids: vec![project],
                    ..Default::default()
                },
                vec!["dispatch".into()],
            ),
            remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
        )
        .await
        .unwrap();
    let sub_token = remuda_hub::instance_token(ctx.store().clone(), sub.instance_id.clone())
        .await
        .unwrap();
    let reply = ctx
        .send(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("grandchild")),
            &sub_token,
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = ctx
        .send(
            "POST",
            "/v1/workers/grandchild/stop",
            Some(json!({})),
            &sibling.token,
            &[],
        )
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    let reply = ctx
        .send(
            "POST",
            "/v1/workers/grandchild/stop",
            Some(json!({})),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}

/// `worker answer` writes keys, and keys cannot stand in for an approval:
/// an Agent's answer on its own worker needs the D-017 one-shot approval for
/// exactly those keys. A retry reuses the same card; nothing is written
/// before the owner allows it.
#[tokio::test]
async fn an_agents_worker_answer_needs_a_human_approval() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let seat = ctx.seat("claude", "auto").await;
    let own = ctx
        .send(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body("mine")),
            &seat.token,
            &[],
        )
        .await;
    assert_eq!(own.status, 200, "{}", own.body);
    ctx.node.drain();
    let answer = || {
        ctx.send(
            "POST",
            "/v1/workers/mine/answer",
            Some(json!({"key": "1"})),
            &seat.token,
            &[],
        )
    };
    let first = answer().await;
    assert_eq!(first.status, 409, "{}", first.body);
    let card = first.body["interactionId"].as_str().unwrap().to_string();
    let again = answer().await;
    assert_eq!(
        again.body["interactionId"],
        card.as_str(),
        "the retry reuses the card"
    );
    assert!(
        !ctx.node.methods().contains(&"tty.write".to_string()),
        "no keys before approval"
    );
    ctx.approve(&card).await;
    let approved = ctx
        .send(
            "POST",
            "/v1/workers/mine/answer",
            Some(json!({"key": "1"})),
            &seat.token,
            &[("x-remuda-approval-id", card.as_str())],
        )
        .await;
    assert_eq!(approved.status, 200, "{}", approved.body);
    let writes: Vec<_> = ctx
        .node
        .drain()
        .into_iter()
        .filter(|(method, _)| method == "tty.write")
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].1["keys"], json!(["1"]));
}
