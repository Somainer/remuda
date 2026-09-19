//! Hub e2e for the D-047/D-048 model-API relay, driven against the offline
//! fake gateway in `remuda-testing`.
//!
//! The shape under test is the in-band one from `docs/design/api-routing.md`
//! §3: a worker's Node accepts a request on its per-instance loopback listener
//! and forwards it as `api.open` / `api.body` over the **existing** Hub↔Node
//! link; the Hub (or the proxy host's Node, when the proxy host is not the Hub
//! host) rebuilds the request against the profile's pinned origin, swaps the
//! credential, and streams the response back as `api.head` / `api.chunk` /
//! `api.end`. Neither leg opens a listener off loopback.
//!
//! ## These frames are notifications, with no ids and no acks
//!
//! That is a hard requirement, not a style choice: `api-routing.md` §4.4 says
//! all seven frames are notifications, each direction keeping its own per-link
//! stream registry, so a hot relay stream **never** consumes the 32-slot
//! in-flight RPC map that `instance.create` and `tty.*` depend on. Every helper
//! here therefore sends and observes *notifications*: a test that waited for a
//! JSON-RPC reply to an `api.*` frame would hang until its timeout, because no
//! reply is ever coming. Ordering is carried by `streamId` and `seq`, not by
//! request/response correlation.
//!
//! ## How these tests run
//!
//! All twelve tests are live: the Hub-side relay router landed with
//! `c-apiroute-hub` (task 2). Every test stands up the in-process Hub and a
//! D-048-capable worker; the Hub-self cases use an explicit `apiVia: self`
//! delivery, and the via-host case dispatches a real worker with
//! `apiVia: H, hub-relay` and a second fake Node as H. The origin is always
//! the shared `FakeGateway` in `remuda-testing`, never a local HTTP stub.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use remuda_testing::fake_gateway::{DEFAULT_TEXT, FakeGateway, Script};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Generous: this box is a shared devbox and the Hub is spun up in-process.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on a single relayed chunk, raw bytes (D-048 `apiChunkBytes`).
const API_CHUNK_BYTES: usize = 64 * 1024;

/// The credential the fixture profile is created with.
///
/// A synthetic value that exists only in this process. No test ever asserts on
/// it directly: the gateway compares it in constant time and reports a boolean,
/// so the value is never printed, copied into output, or written to a file.
const PROFILE_TOKEN: &str = "sk-fake-profile-0001";

/// The worker's per-instance relay bearer — the value a worker holds, and the
/// one that must **never** reach the gateway.
///
/// Used to keep the two credentials distinguishable: the origin is configured
/// with [`PROFILE_TOKEN`], so a relay that forwarded *this* value instead
/// presents a credential that does not match and is refused. The value is never
/// sent anywhere in these tests and never printed; only the mismatch verdict is
/// read.
const WORKER_RELAY_BEARER: &str = "fake-worker-bearer-0002";

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ── Frame plumbing ─────────────────────────────────────────────────────────

async fn recv_json(ws: &mut NodeSocket) -> Result<Value> {
    loop {
        let message = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected frame {other:?}")),
        }
    }
}

async fn send_json(node: &mut NodeSocket, frame: Value) -> Result<()> {
    node.send(Message::Text(frame.to_string().into())).await?;
    Ok(())
}

/// Send an `api.*` **notification**: no `id`, no reply expected or possible.
async fn notify(node: &mut NodeSocket, method: &str, params: Value) -> Result<()> {
    send_json(
        node,
        json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
    .await
}

/// Send an ordinary JSON-RPC **request** and read until its matching reply.
///
/// Used only for the control-plane methods (`node.hello`, `journal.append`,
/// `tty.frame`), which _are_ requests and do reply. Any relay notification that
/// arrives while waiting is handed to `collect`.
async fn request<F>(
    node: &mut NodeSocket,
    frame: Value,
    mut collect: F,
) -> Result<(Value, Vec<Value>)>
where
    F: FnMut(&Value) -> bool,
{
    let rpc_id = frame["id"].clone();
    send_json(node, frame).await?;
    let mut notifications = Vec::new();
    loop {
        let reply = recv_json(node).await?;
        if reply.get("method").is_some() {
            if collect(&reply) {
                notifications.push(reply["params"].clone());
            }
            continue;
        }
        if reply["id"] == rpc_id {
            return Ok((reply, notifications));
        }
    }
}

/// Read notifications off one node until `stop` matches one (or the budget runs
/// out), returning everything seen.
///
/// The relay's frames are all notifications, so this — not a reply — is how a
/// test observes the response leg.
async fn collect_notifications<F>(
    node: &mut NodeSocket,
    budget: Duration,
    mut stop: F,
) -> Result<Vec<Value>>
where
    F: FnMut(&Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), recv_json(node)).await {
            Ok(Ok(frame)) => {
                let done = stop(&frame);
                seen.push(frame);
                if done {
                    return Ok(seen);
                }
            }
            Ok(Err(err)) => return Err(err),
            // Quiet: not an error, just nothing more to see yet.
            Err(_) => continue,
        }
    }
    Ok(seen)
}

/// Every `api.*` frame the node received for `stream_id`.
fn frames_for(frames: &[Value], stream_id: &str) -> Vec<Value> {
    frames
        .iter()
        .filter(|frame| {
            frame["method"]
                .as_str()
                .is_some_and(|method| method.starts_with("api."))
                && frame["params"]["streamId"] == json!(stream_id)
        })
        .map(|frame| frame["params"].clone())
        .collect()
}

// ── Fixtures ───────────────────────────────────────────────────────────────

/// A booted Hub, a connected fake Node, and a projected running instance.
struct Fixture {
    addr: std::net::SocketAddr,
    cookie: String,
    node: NodeSocket,
    instance_id: String,
    _hub: remuda_hub::RunningHub,
    _dir: tempfile::TempDir,
}

/// Boot a Hub, mint an enroll token, connect a fake Node, and project one
/// running instance onto it.
async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;

    // Note: `upgrade`, not `request` — `request` is the RPC helper above.
    let mut upgrade = format!("ws://{addr}/v1/node").into_client_request()?;
    upgrade
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(upgrade)).await??;

    let host_id = HostId::new().as_id().as_str().to_owned();
    send_json(
        &mut node,
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.1.0", "label":"relay-worker"}}),
    )
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.pointer("/result/hostId").is_some(), "{hello}");

    // Project a running instance onto **this** host, so the Hub can authorize
    // the relay against a real instance row (it re-checks the Node's word,
    // exactly as for object.pull).
    let instance_id = InstanceId::new().as_id().as_str().to_owned();
    let (ack, _) = request(
        &mut node,
        json!({"jsonrpc":"2.0", "id":"s1", "method":"journal.append",
            "params":{"instanceId":instance_id, "event":{
                "kind":"lifecycle",
                "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                           "reasonCode":"driver-started"}}}}),
        |_| false,
    )
    .await?;
    anyhow::ensure!(ack.get("result").is_some(), "instance projection: {ack}");

    Ok(Fixture {
        addr,
        cookie,
        node,
        instance_id,
        _hub: hub,
        _dir: dir,
    })
}

/// Boot a Hub with a D-048-capable worker and the given bootstrap token.
async fn boot_routed() -> Result<RoutedHub> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    // Launch-time RPC races the test's answer loop; give it headroom.
    config.command_accept_timeout_ms = 15_000;
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    Ok(RoutedHub {
        addr: hub.addr,
        cookie,
        _hub: hub,
        _dir: dir,
    })
}

struct RoutedHub {
    addr: std::net::SocketAddr,
    cookie: String,
    _hub: remuda_hub::RunningHub,
    _dir: tempfile::TempDir,
}

/// Connect a worker fake that advertises the D-048 relay capability and (when
/// `with_workspace`) registers one workspace + reports herdr-less inventory.
///
/// The socket stays with the caller: create/dispatch are answered inline by
/// [`answer_create`] / [`answer_dispatch`], after which only relay
/// notifications flow.
async fn connect_capable_node(
    addr: std::net::SocketAddr,
    hub: &remuda_hub::RunningHub,
    label: &str,
    with_workspace: bool,
) -> Result<(NodeSocket, String, Option<String>, Option<String>)> {
    connect_capable_node_as(addr, hub, label, with_workspace, None).await
}

/// Like [`connect_capable_node`] but allows presenting an existing host id
/// with a fresh enroll token (the Hub's re-enrollment rule rejects this for
/// existing hosts — reconnection must use the Node token from the first hello).
async fn connect_capable_node_as(
    addr: std::net::SocketAddr,
    hub: &remuda_hub::RunningHub,
    label: &str,
    with_workspace: bool,
    existing_host_id: Option<&str>,
) -> Result<(NodeSocket, String, Option<String>, Option<String>)> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut upgrade = format!("ws://{addr}/v1/node").into_client_request()?;
    upgrade
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(upgrade)).await??;
    let host_id = existing_host_id
        .map(str::to_string)
        .unwrap_or_else(|| HostId::new().as_id().as_str().to_owned());
    let workspace_id = if with_workspace {
        Some(
            remuda_protocol::WorkspaceId::new()
                .as_id()
                .as_str()
                .to_owned(),
        )
    } else {
        None
    };
    let workspace = match &workspace_id {
        Some(id) => json!([{
            "workspaceId": id,
            "hostId": host_id,
            "root": "/tmp/relay-ws"
        }]),
        None => json!([]),
    };
    send_json(
        &mut node,
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.2.0-d048", "label": label,
                       "capabilities": {"apiRelay": true, "features": ["api-relay-v1"]},
                       "host": {"maxInstances": 8, "workspaces": workspace,
                                "workspaceRevision": 1,
                                "cli": [{"kind":"claude","auth":"gateway-logged-in"}]}}}),
    )
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.pointer("/result/hostId").is_some(), "{hello}");
    // First enrollment hands back the Node's durable re-auth token.
    let node_token = hello
        .pointer("/result/nodeToken")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((node, host_id, workspace_id, node_token))
}

/// Reconnect a fake Node under an existing host id (same registry row) and
/// re-advertise the D-048 capability. Used when a launch-time answer task had
/// to own the socket and was aborted.
async fn reconnect_worker(
    addr: std::net::SocketAddr,
    _hub: &remuda_hub::RunningHub,
    host_id: &str,
    label: &str,
    node_token: &str,
) -> Result<NodeSocket> {
    let mut upgrade = format!("ws://{addr}/v1/node").into_client_request()?;
    upgrade
        .headers_mut()
        .insert("Authorization", format!("Bearer {node_token}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(upgrade)).await??;
    send_json(
        &mut node,
        json!({"jsonrpc":"2.0", "id":"hello2", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.2.0-d048", "label": label,
                       "capabilities": {"apiRelay": true, "features": ["api-relay-v1"]}}}),
    )
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.pointer("/result/hostId").is_some(), "{hello}");
    Ok(node)
}

/// Answer the one `instance.create` RPC a plain create produces, echoing the
/// route the Hub requested (the truthful D-035 observation).
async fn answer_create(node: &mut NodeSocket) -> Result<Value> {
    let frame = recv_json(node).await?;
    anyhow::ensure!(frame["method"] == json!("instance.create"), "got {frame}");
    let id = frame["id"].clone();
    let params = frame.get("params").cloned().unwrap_or(json!({}));
    let mut result = json!({
        "accepted": true,
        "instanceId": params.get("instanceId").cloned().unwrap_or(Value::Null),
        "instance": { "driver": params.pointer("/spec/driver").cloned().unwrap_or(json!("claude-print")) }
    });
    if let Some(route) = params.pointer("/spec/apiRoute").cloned() {
        result["apiRoute"] = route;
    }
    send_json(node, json!({"jsonrpc":"2.0", "id": id, "result": result})).await?;
    Ok(result)
}

/// Answer launch-cycle RPCs as they arrive, never timing out: the dispatch
/// HTTP call is parked until the create reply lands, and this loop must not
/// stop reading before then. Runs until the task is aborted.
///
/// Read and write happen on separate tasks: `send().await` only resolves once
/// the frame leaves the 32-slot outbound FIFO, and a single-task read/send
/// loop can stall the create reply behind a backlogged earlier control frame.
/// The writer task keeps draining replies independently.
async fn answer_dispatch_eager(node: NodeSocket) -> Result<()> {
    use futures::stream::StreamExt;
    let (sink, stream) = node.split();
    let (reply_tx, reply_rx) = mpsc::unbounded_channel::<Value>();
    let reader = tokio::spawn({
        let reply_tx = reply_tx.clone();
        async move {
            futures::pin_mut!(stream);
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if frame.get("method").is_some() {
                    let _ = reply_tx.send(frame);
                }
            }
        }
    });
    let writer = tokio::spawn({
        let mut reply_rx = reply_rx;
        async move {
            let mut sink = sink;
            while let Some(frame) = reply_rx.recv().await {
                let id = frame["id"].clone();
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                let params = frame.get("params").cloned().unwrap_or(json!({}));
                let result = match method {
                    "worker.provision" => json!({
                        "name": "relay-worker",
                        "branch": "wt/relay/work",
                        "startPoint": "origin/main",
                        "worktreePath": "/tmp/remuda-wt/relay",
                        "targetDir": "/tmp/remuda-target/relay"
                    }),
                    "worker.remove" => json!({ "worktreeRemoved": true, "targetRemoved": true }),
                    "instance.create" => {
                        let mut result = json!({
                            "accepted": true,
                            "instanceId": params.get("instanceId").cloned().unwrap_or(Value::Null),
                            "instance": { "driver": "claude-print" }
                        });
                        if let Some(route) = params.pointer("/spec/apiRoute").cloned() {
                            // The observation echoes the route the Node
                            // actually ended up on: this fake cannot probe
                            // direct-net, so a requested `auto` reports the
                            // hub-relay fallback (ApiRoute has no `auto` kind).
                            let mut observed = route;
                            if observed.get("route").and_then(Value::as_str) == Some("auto") {
                                observed["route"] = json!("hub-relay");
                            }
                            result["apiRoute"] = observed;
                        }
                        result
                    }
                    _ => json!({ "ok": true }),
                };
                let reply = json!({"jsonrpc":"2.0", "id": id, "result": result});
                if sink
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    let _ = (reader, writer);
    Ok(())
}

/// A Fixture whose instance was created through the real HTTP surface with an
/// explicit **delivery configuration** — here `apiVia: self`, so the Hub
/// itself egresses to the pinned origin.
async fn fixture_routed_self(base_url: &str) -> Result<Fixture> {
    let hub = boot_routed().await?;
    let profile_id = create_profile(hub.addr, &hub.cookie, base_url).await?;
    let (mut node, host_id, _workspace, _node_token) =
        connect_capable_node(hub.addr, &hub._hub, "relay-self", false).await?;

    let body = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-print",
        "permissionMode": "bypassPermissions",
        "delegation": "gateway",
        "providerProfileId": profile_id,
        "prompt": "relay me",
        "apiVia": "self",
        "apiRoute": "hub-relay"
    });
    let instance_id = create_with_inline_answer(hub.addr, &hub.cookie, &mut node, body).await?;
    Ok(Fixture {
        addr: hub.addr,
        cookie: hub.cookie,
        node,
        instance_id,
        _hub: hub._hub,
        _dir: hub._dir,
    })
}

/// POST /v1/instances while answering the create RPC on the worker socket.
async fn create_with_inline_answer(
    addr: std::net::SocketAddr,
    cookie: &str,
    node: &mut NodeSocket,
    body: Value,
) -> Result<String> {
    let body = body.to_string();
    let cookie = cookie.to_string();
    let post = async {
        http(
            addr,
            "POST",
            "/v1/instances",
            &[("Cookie", cookie.as_str())],
            Some(&body),
        )
        .await
    };
    tokio::pin!(post);
    // Poll the POST to readiness concurrently with answering the create RPC;
    // the HTTP handler parks on the Node reply, which arrives below.
    let echo_fut = answer_create(node);
    let response;
    tokio::select! {
        biased;
        r = &mut post => response = Some(r?),
        echo = echo_fut => {
            let echo = echo?;
            response = Some(post.await?);
            let _ = echo;
        }
    }
    let (status, _, rest) = response.context("post never resolved")?;
    anyhow::ensure!(status == 200, "create {status} {rest}");
    let created: Value = serde_json::from_str(rest.trim())?;
    created["instance"]["instanceId"]
        .as_str()
        .map(str::to_string)
        .context("instanceId")
}

/// A Fixture whose instance is a *dispatched worker* routed `via:<H>`:
/// the second Node H is the proxy, and a roster row exists so a lost H can be
/// observed as `api-route-down`.
async fn fixture_routed_via_h(base_url: &str) -> Result<ViaH> {
    fixture_routed_via_h_with(base_url, ViaOpts::default()).await
}

/// Launch options for the via-H fixture (item 3 exercises `auto`).
#[derive(Clone)]
struct ViaOpts {
    /// Requested `apiRoute` sub-mode: `hub-relay` | `auto` | `direct-net`.
    route: &'static str,
    /// Patch a private `relayBind` onto H before dispatch, so a requested
    /// `auto`/`direct-net` keeps its sub-mode at validation instead of
    /// collapsing to hub-relay.
    relay_bind: bool,
}

impl Default for ViaOpts {
    fn default() -> Self {
        Self {
            route: "hub-relay",
            relay_bind: false,
        }
    }
}

/// [`fixture_routed_via_h`] with control over the requested sub-mode and H's
/// relayBind.
async fn fixture_routed_via_h_with(base_url: &str, opts: ViaOpts) -> Result<ViaH> {
    let hub = boot_routed().await?;
    let _profile_id = create_profile(hub.addr, &hub.cookie, base_url).await?;
    let (worker, worker_host, workspace_id, worker_node_token) =
        connect_capable_node_as(hub.addr, &hub._hub, "relay-worker", true, None).await?;
    let workspace_id = workspace_id.context("worker registered a workspace")?;
    let worker_node_token = worker_node_token.context("worker hello issued a node token")?;
    let proxy = ProxyNode::connect(
        hub.addr,
        &hub._hub
            .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?,
        "relay-proxy",
    )
    .await?;
    let proxy_host = proxy.host_id.clone();

    // Project with W as its only member; portBlocks keeps the allocation real.
    let project_body = json!({
        "name": "relay-via-h",
        "members": [
            { "hostId": worker_host, "workspaceId": workspace_id, "role": "build" }
        ],
        "hosts": [{
            "hostId": worker_host,
            "maxInstances": 8,
            "latencyClass": "remote",
            "portBlocks": ["58600-58629"]
        }]
    })
    .to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/projects",
        &[("Cookie", &hub.cookie)],
        Some(&project_body),
    )
    .await?;
    anyhow::ensure!(status == 200, "project {status} {rest}");
    let project: Value = serde_json::from_str(rest.trim())?;
    let project_id = project["id"].as_str().context("project id")?;

    // Item 3: a private relayBind on H keeps `auto`/`direct-net` resolved as
    // requested (validation collapses auto to hub-relay only without one).
    if opts.relay_bind {
        let (status, _, rest) = http(
            hub.addr,
            "PATCH",
            &format!("/v1/hosts/{proxy_host}"),
            &[("Cookie", &hub.cookie)],
            Some(
                &json!({
                    "relayBind": {"addr": "10.0.0.2:8443", "allowFrom": ["10.0.0.0/8"]}
                })
                .to_string(),
            ),
        )
        .await?;
        anyhow::ensure!(status == 200, "relayBind patch {status} {rest}");
    }

    let dispatch_body = json!({
        "projectId": project_id,
        "brief": "relay the model API please",
        "harness": "claude",
        "driver": "claude-print",
        "apiVia": proxy_host,
        "apiRoute": opts.route
    })
    .to_string();
    let cookie = hub.cookie.clone();
    let (ready_tx, mut ready_rx) = mpsc::unbounded_channel::<()>();
    // Answer the dispatch RPCs on a spawned task that owns the socket. The
    // HTTP call parks until the create reply lands; once it returns the task
    // is aborted (abort drops the socket), so a fresh socket is needed for the
    // relay frames — reconnect as the same host id and re-hello.
    let answer_task = tokio::spawn(async move {
        ready_tx.send(()).ok();
        answer_dispatch_eager(worker).await
    });
    // Block until the spawned answer task is scheduled and past its first poll
    // (the ready send happens before it starts blocking on socket reads), so
    // the HTTP call cannot start parking on an RPC nobody is reading yet.
    ready_rx.recv().await.context("ready signal")?;
    // Poll the dispatch call only after the answer task owns the socket.
    let dispatch = async move {
        http(
            hub.addr,
            "POST",
            "/v1/workers/dispatch",
            &[("Cookie", cookie.as_str())],
            Some(&dispatch_body),
        )
        .await
    };
    let (status, _, rest): (u16, String, String) = dispatch.await?;
    anyhow::ensure!(status == 200, "dispatch {status} {rest}");
    answer_task.abort();
    // Give the answer task's own shutdown a moment, then reconnect W into a
    // fresh binding.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let worker = reconnect_worker(
        hub.addr,
        &hub._hub,
        &worker_host,
        "relay-worker",
        &worker_node_token,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dispatched: Value = serde_json::from_str(rest.trim())?;
    let instance_id = dispatched["worker"]["instanceId"]
        .as_str()
        .context("dispatched instance id")?
        .to_string();

    // H stays connected until the test drops it; the Fixture holds W.
    Ok((
        Fixture {
            addr: hub.addr,
            cookie: hub.cookie,
            node: worker,
            instance_id,
            _hub: hub._hub,
            _dir: hub._dir,
        },
        proxy,
    ))
}

/// Returned by [`fixture_routed_via_h`]: the worker-side fixture and the
/// live proxy host H (drop to take offline; `reconnect` to re-hello).
type ViaH = (Fixture, ProxyNode);

/// A second fake Node, playing the proxy host H.
///
/// The plan's §(C) task 2 asks for a *pair* of fake Nodes: `via:<H>` means the
/// worker's Node hands the request to the Hub and the Hub hands it to H, so a
/// test that only ever had one Node could not tell which machine actually did
/// the egress — nor drive "H went offline mid-stream", which is a failure about
/// a *remote* link rather than the worker's own. The two tests that need it
/// (`a_via_host_lost_mid_stream_…`) connect one of these and then drop it.
struct ProxyNode {
    host_id: String,
    /// Durable Node token from the first hello; used to re-authenticate a
    /// replacement socket after a drop.
    node_token: String,
    /// Frames read while waiting for the hello reply on reconnect.
    pending: Vec<Value>,
    /// Held so the connection lives as long as the value does; `take_link`
    /// drops it to take H offline.
    node: Option<NodeSocket>,
}

impl ProxyNode {
    /// Connect a second Node and complete its hello, returning the live link.
    ///
    /// Enrollment comes from the Hub's own mint, exactly as the worker's does —
    /// H is an ordinary enrolled host, not a special case.
    async fn connect(addr: std::net::SocketAddr, enroll: &str, label: &str) -> Result<Self> {
        let mut upgrade = format!("ws://{addr}/v1/node").into_client_request()?;
        upgrade
            .headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut node, _) =
            tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(upgrade)).await??;
        let host_id = HostId::new().as_id().as_str().to_owned();
        send_json(
            &mut node,
            json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params":{"hostId": host_id, "nodeVersion":"0.2.0-d048", "label": label,
                           "capabilities": {"apiRelay": true, "features": ["api-relay-v1"]}}}),
        )
        .await?;
        let reply = recv_json(&mut node).await?;
        anyhow::ensure!(reply.pointer("/result/hostId").is_some(), "{reply}");
        let node_token = reply
            .pointer("/result/nodeToken")
            .and_then(Value::as_str)
            .context("proxy hello issued a nodeToken")?
            .to_string();
        Ok(Self {
            host_id,
            node_token,
            pending: Vec::new(),
            node: Some(node),
        })
    }

    /// Drop H's live link (takes it offline); reconnect re-installs one.
    fn drop_link(&mut self) {
        self.node = None;
    }

    /// Frames received while the reconnect reader was waiting for the hello
    /// reply (api.egress can be queued ahead of the reply).
    fn take_pending(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending)
    }

    /// Mutable access to the live socket.
    fn socket(&mut self) -> &mut NodeSocket {
        self.node.as_mut().expect("proxy link is down")
    }

    /// Open a replacement socket for the same host id using the durable Node
    /// token from the first hello.
    async fn reconnect(&mut self, addr: std::net::SocketAddr, label: &str) -> Result<()> {
        let mut upgrade = format!("ws://{addr}/v1/node").into_client_request()?;
        upgrade.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.node_token).parse()?,
        );
        let (mut node, _) =
            tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(upgrade)).await??;
        send_json(
            &mut node,
            json!({"jsonrpc":"2.0", "id":"hello2", "method":"node.hello",
                "params":{"hostId": self.host_id, "nodeVersion":"0.2.0-d048", "label": label,
                           "capabilities": {"apiRelay": true, "features": ["api-relay-v1"]}}}),
        )
        .await?;
        // The Hub may push api.egress (and other notifications) before the
        // hello reply; retain them in `pending` rather than dropping them.
        loop {
            let reply = recv_json(&mut node).await?;
            if reply.get("id").and_then(Value::as_str) == Some("hello2") {
                anyhow::ensure!(reply.pointer("/result/hostId").is_some(), "{reply}");
                break;
            }
            self.pending.push(reply);
        }
        self.node = Some(node);
        Ok(())
    }
}

/// Poll until the Hub reports `host_id` offline, using a raw HTTP GET.
async fn wait_proxy_host_offline(fixture: &Fixture, host_id: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let (_, _, body) = http(
            fixture.addr,
            "GET",
            &format!("/v1/hosts/{host_id}"),
            &[("Cookie", &fixture.cookie)],
            None,
        )
        .await?;
        let body: Value = serde_json::from_str(&body).unwrap_or(json!(null));
        if body["online"] == json!(false) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("host {host_id} never went offline")
}

// ── HTTP helpers ───────────────────────────────────────────────────────────

/// Minimal HTTP client: one request, `Connection: close`, head and body back.
///
/// Returns `(status, head, body)` — the head is separate because the login
/// cookie only exists there, and the body because every other route answers in
/// JSON.
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(body) = body {
        request.extend_from_slice(body.as_bytes());
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "api-relay-test" }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':')?.1.split(';').next())
        .map(|value| value.trim().to_string())
        .context("login set-cookie")
}

/// Look up the (single) relay-fixture profile id for supply-evidence checks.
async fn fixture_profile_id(fixture: &Fixture) -> Result<String> {
    let (_, _, rest) = http(
        fixture.addr,
        "GET",
        "/v1/providers",
        &[("Cookie", fixture.cookie.as_str())],
        None,
    )
    .await?;
    let providers: Value = serde_json::from_str(rest.trim())?;
    providers["items"][0]["id"]
        .as_str()
        .map(str::to_string)
        .context("fixture profile id")
}

/// Create a gateway profile pointing at `base_url` and return its id.
///
/// The profile carries [`PROFILE_TOKEN`]; no test reads it back out of the Hub,
/// and the gateway only ever reports whether the credential it received matched.
///
/// Note: this does **not** set a delivery mode. The provider create/patch
/// surface has no `delivery` field yet — that arrives with `c-apiroute-hub`
/// (task 2), together with the `apiVia` waterfall. The ignored tests below
/// therefore drive the relay at the frame level, which is the layer this file
/// owns.
async fn create_profile(
    addr: std::net::SocketAddr,
    cookie: &str,
    base_url: &str,
) -> Result<String> {
    let body = json!({
        "name": "relay-fixture",
        "kind": "gateway",
        "baseUrl": base_url,
        "models": ["fake/model-1"],
        "defaultModel": "fake/model-1",
        "authToken": PROFILE_TOKEN,
        "defaultGateway": true,
    })
    .to_string();
    let (status, _, rest) = http(
        addr,
        "POST",
        "/v1/providers",
        &[("Cookie", cookie)],
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "create profile {status} {rest}");
    let created: Value = serde_json::from_str(rest.trim())?;
    created["id"]
        .as_str()
        .map(str::to_string)
        .context("profile id")
}

// ── api.* params ───────────────────────────────────────────────────────────

/// The `api.open` params a worker's Node sends for one Messages request.
///
/// Built as raw JSON rather than from `remuda_protocol::ApiOpenParams` so this
/// file reads as the wire contract itself; the shapes are identical and the Hub
/// parses node frames as `serde_json::Value` regardless. Swapping in the typed
/// params is a local change confined to this helper if that is ever preferred.
fn api_open_params(instance_id: &str, stream_id: &str, body: &str) -> Value {
    json!({
        "instanceId": instance_id,
        "streamId": stream_id,
        "method": "POST",
        "path": "/v1/messages",
        "query": "",
        "headers": [
            { "name": "content-type", "value": "application/json" },
            { "name": "anthropic-version", "value": "2023-06-01" },
            { "name": "x-stainless-lang", "value": "js" },
        ],
        "bodyBase64": base64_encode(body.as_bytes()),
        "bodyChunked": false,
        "deadlineMs": 30_000,
    })
}

/// An `api.body` continuation frame's params.
fn api_body_params(stream_id: &str, seq: usize, slice: &[u8], last: bool) -> Value {
    json!({
        "streamId": stream_id,
        "seq": seq,
        "dataBase64": base64_encode(slice),
        "last": last,
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(value)?)
}

/// A Messages request body, as a client would send it.
fn messages_body() -> String {
    json!({
        "model": "fake/model-1",
        "max_tokens": 64,
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    })
    .to_string()
}

/// Reassemble the `api.chunk` payloads of one stream, in `seq` order.
///
/// Coalescing preserves byte order exactly (D-048), so sorting by `seq` and
/// concatenating must reproduce the origin's body byte for byte.
fn reassemble_chunks(frames: &[Value]) -> Result<Vec<u8>> {
    let mut chunks: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame.get("dataBase64").is_some() && frame.get("bytesDown").is_none())
        .collect();
    chunks.sort_by_key(|frame| frame["seq"].as_u64().unwrap_or(0));
    let mut body = Vec::new();
    for chunk in chunks {
        body.extend_from_slice(&base64_decode(
            chunk["dataBase64"].as_str().context("dataBase64")?,
        )?);
    }
    Ok(body)
}

/// Reassemble the `text_delta` payloads of an SSE body into one string.
///
/// Returns an error on a malformed frame rather than skipping it: a frame the
/// relay mangled is exactly what the caller is looking for.
fn reassemble_deltas(body: &str) -> Result<String> {
    let mut out = String::new();
    for block in body.split("\n\n") {
        let mut event = None;
        let mut data = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = Some(rest);
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = Some(rest);
            }
        }
        if event != Some("content_block_delta") {
            continue;
        }
        let value: Value = serde_json::from_str(data.context("delta without data")?)?;
        out.push_str(
            value
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
    }
    Ok(out)
}

/// The `api.head` params of the first response head for `stream_id`.
fn api_head(frames: &[Value], stream_id: &str) -> Option<Value> {
    frames_for(frames, stream_id)
        .into_iter()
        .find(|params| params.get("status").is_some())
}

/// The header names on an `api.head`'s `headers` list, lowercased.
fn head_header_names(head: &Value) -> Vec<String> {
    head["headers"]
        .as_array()
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| header["name"].as_str().map(str::to_ascii_lowercase))
                .collect()
        })
        .unwrap_or_default()
}

/// Assert the origin saw exactly the profile credential and nothing else.
///
/// The relay's real credential assertion, and the reason the fixture compares
/// values: a relay that forwarded the worker's per-instance bearer instead
/// would present a *different* credential, which the gateway answers `401` and
/// reports as a mismatch. Both values stay inside the fixture — this only reads
/// verdicts.
fn assert_profile_credential_swapped(gateway: &FakeGateway) -> Result<()> {
    gateway
        .assert_presented_expected_credential()
        .context("the relay must present the profile credential, not the worker's bearer")?;
    anyhow::ensure!(
        !gateway.saw_credential_mismatch(),
        "a credential other than the profile's reached the origin: {:?}",
        gateway.credential_violations()
    );
    Ok(())
}

// ── The facts the relay is built on ────────────────────────────────────────

/// An `api.open` for an instance with no via route is nothing to relay: the
/// router refuses it, but the refusal is delivered on the stream and the
/// control plane on the same socket keeps working.
///
/// With the router landed (c-apiroute-hub) this is no longer "the frame is
/// silently ignored": the instance projected here has no `apiRoute`, so the
/// open is rejected at second authorization. What stays pinned is the part
/// that always mattered — a bad relay notification never tears down the link.
#[tokio::test]
async fn an_unroutable_api_open_does_not_break_the_link() -> Result<()> {
    let mut fixture = fixture().await?;
    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_probe", &messages_body()),
    )
    .await?;

    // The control plane is untouched by the unroutable notification.
    let (ack, _) = request(
        &mut fixture.node,
        json!({"jsonrpc":"2.0", "id":"alive", "method":"journal.append",
            "params":{"instanceId": fixture.instance_id, "event":{
                "kind":"lifecycle",
                "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                           "reasonCode":"driver-started"}}}}),
        |_| false,
    )
    .await?;
    assert!(
        ack.get("result").is_some(),
        "an unroutable api.* notification must not break the link: {ack}"
    );
    Ok(())
}

/// Item 12: every open-time rejection must terminate the stream with a named
/// `destination-refused` (or `instance-gone`) api.end, never a silent drop.
#[tokio::test]
async fn open_time_rejections_all_emit_a_terminal_end() -> Result<()> {
    let gateway = FakeGateway::start().await?;

    // Self-routed instance with a gateway profile: unknown / wrong host /
    // bad path / duplicate / per-link cap all exercise real checks.
    let mut routed = fixture_routed_self(&gateway.base_url_v1()).await?;
    let other = connect_capable_node(routed.addr, &routed._hub, "other-node", false).await?;
    let mut other_socket = other.0;

    // Unknown instance.
    notify(
        &mut routed.node,
        "api.open",
        api_open_params("ins_does_not_exist", "st_unknown", &messages_body()),
    )
    .await?;
    assert_end_code(&mut routed.node, "st_unknown", "instance-gone").await?;

    // Wrong host: open for routed's instance from the other Node's socket.
    notify(
        &mut other_socket,
        "api.open",
        api_open_params(&routed.instance_id, "st_wronghost", &messages_body()),
    )
    .await?;
    assert_end_code(&mut other_socket, "st_wronghost", "destination-refused").await?;

    // Bad path (origin traversal).
    let mut badpath = api_open_params(&routed.instance_id, "st_badpath", &messages_body());
    badpath["path"] = json!("/v1/../../etc/passwd");
    notify(&mut routed.node, "api.open", badpath).await?;
    assert_end_code(&mut routed.node, "st_badpath", "destination-refused").await?;

    // Duplicate stream id: the second of two opens is refused.
    notify(
        &mut routed.node,
        "api.open",
        api_open_params(&routed.instance_id, "st_dup", &messages_body()),
    )
    .await?;
    notify(
        &mut routed.node,
        "api.open",
        api_open_params(&routed.instance_id, "st_dup", &messages_body()),
    )
    .await?;
    assert_end_code(&mut routed.node, "st_dup", "destination-refused").await?;

    // No via route: a plain fixture (no profile / delivery) refuses api.open.
    let mut no_route = fixture().await?;
    notify(
        &mut no_route.node,
        "api.open",
        api_open_params(&no_route.instance_id, "st_noroute", &messages_body()),
    )
    .await?;
    assert_end_code(&mut no_route.node, "st_noroute", "destination-refused").await?;

    // Direct-net: an observed direct-net route never traverses the Hub; an
    // api.open arriving in-band is a misread echo and refused, not rerouted.
    // The bind keeps `direct-net` resolvable at dispatch; the Node truthfully
    // echoes it as the observed route.
    let (mut direct, direct_proxy) = fixture_routed_via_h_with(
        &gateway.base_url_v1(),
        ViaOpts {
            route: "direct-net",
            relay_bind: true,
        },
    )
    .await?;
    notify(
        &mut direct.node,
        "api.open",
        api_open_params(&direct.instance_id, "st_directnet", &messages_body()),
    )
    .await?;
    assert_end_code(&mut direct.node, "st_directnet", "destination-refused").await?;
    drop(direct_proxy);

    // Missing profile: a via instance whose gateway profile was deleted after
    // launch must be refused at open (the credential is gone), never opened
    // against a bare base URL.
    let (mut proficeless, proficeless_proxy) = fixture_routed_via_h(&gateway.base_url_v1()).await?;
    let (status, _, instance_json) = http(
        proficeless.addr,
        "GET",
        &format!("/v1/instances/{}", proficeless.instance_id),
        &[("Cookie", &proficeless.cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "get instance {status} {instance_json}");
    let instance_view: Value = serde_json::from_str(instance_json.trim())?;
    let profile_id = instance_view
        .pointer("/instance/providerProfileId")
        .or_else(|| instance_view.pointer("/providerProfileId"))
        .and_then(Value::as_str)
        .with_context(|| format!("projected providerProfileId in {instance_view}"))?
        .to_string();
    let (status, _, rest) = http(
        proficeless.addr,
        "DELETE",
        &format!("/v1/providers/{profile_id}"),
        &[("Cookie", &proficeless.cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "delete profile {status} {rest}");
    notify(
        &mut proficeless.node,
        "api.open",
        api_open_params(&proficeless.instance_id, "st_noprofile", &messages_body()),
    )
    .await?;
    assert_end_code(&mut proficeless.node, "st_noprofile", "destination-refused").await?;
    drop(proficeless_proxy);

    // Per-link cap (maxApiStreams = 8): ten opens on a FRESH link, the 9th
    // and 10th refused. Use a gateway that holds every response open so the
    // first 8 streams stay registered while the remaining two are checked.
    let cap_gateway = FakeGateway::start_with(vec![Script::SlowFirstByte {
        delay: Duration::from_secs(10),
        text: "held".into(),
    }])
    .await?;
    let mut capped = fixture_routed_self(&cap_gateway.base_url_v1()).await?;

    // Per-instance cap (2 streams): the third concurrent open on the same
    // instance is refused even though the link still has free slots.
    for i in 0..3u32 {
        notify(
            &mut capped.node,
            "api.open",
            api_open_params(
                &capped.instance_id,
                &format!("st_inst_{i}"),
                &messages_body(),
            ),
        )
        .await?;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_end_code(&mut capped.node, "st_inst_2", "destination-refused").await?;

    // Two slots are taken by st_inst_0/1, so the link cap bites after six
    // more (the 9th/10th of these ten are refused).
    for i in 0..10u32 {
        notify(
            &mut capped.node,
            "api.open",
            api_open_params(
                &capped.instance_id,
                &format!("st_cap_{i}"),
                &messages_body(),
            ),
        )
        .await?;
    }
    // Give the drain queue a moment to process all ten queued opens.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_end_code(&mut capped.node, "st_cap_8", "destination-refused").await?;
    assert_end_code(&mut capped.node, "st_cap_9", "destination-refused").await?;
    cap_gateway.shutdown().await;

    gateway.shutdown().await;
    Ok(())
}

/// Read until `stream_id` terminates with `api.end{error.code == code}`.
async fn assert_end_code(node: &mut NodeSocket, stream_id: &str, code: &str) -> Result<()> {
    let frames = collect_notifications(node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!(stream_id)
            && frame["params"].get("bytesDown").is_some()
            && frame["params"].get("error").is_some()
    })
    .await?;
    let end = frames_for(&frames, stream_id)
        .into_iter()
        .find(|p| p.get("error").is_some())
        .with_context(|| format!("{stream_id} must end with an error"))?;
    assert_eq!(end["error"]["code"], json!(code), "{stream_id}: {end}");
    Ok(())
}

/// The relay's destination is a real, streaming Messages origin.
///
/// This is the one test that exercises the fake gateway over the same loopback
/// path a relay takes, and it needs no Hub relay code: it speaks HTTP to the
/// gateway directly. It fails if the fixture and the relay disagree about what
/// the origin does — SSE framing, the `anthropic-version` header, or the
/// two-catalog listing.
#[tokio::test]
async fn the_relay_destination_streams_sse_and_lists_two_catalogs() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let client = reqwest::Client::builder().timeout(TIMEOUT).build()?;

    // A credential-free request still streams, which is what lets a relay test
    // assert on the credential *swap* rather than on a credential.
    let response = client
        .post(format!("{}/v1/messages", gateway.base_url()))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(messages_body())
        .send()
        .await
        .context("POST /v1/messages")?;
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream")),
        "the origin must stream: {:?}",
        response.headers()
    );
    let body = response.text().await?;
    assert!(body.contains("event: message_start"), "{body}");
    assert!(body.contains("event: message_stop"), "{body}");
    // The text arrives split across `text_delta` events, so the way to check it
    // is to reassemble the deltas — which is also what proves the framing is
    // lossless rather than merely well-formed.
    assert_eq!(reassemble_deltas(&body)?, DEFAULT_TEXT);

    // The two listings overlap without nesting, so a probe that read one
    // surface under-reports — the reason `/discover` unions both.
    let plain = model_ids(
        &client
            .get(format!("{}/v1/models", gateway.base_url()))
            .send()
            .await?
            .json::<Value>()
            .await?,
    )?;
    let anthropic = model_ids(
        &client
            .get(format!("{}/v1/models", gateway.base_url()))
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?
            .json::<Value>()
            .await?,
    )?;
    assert!(
        plain.iter().any(|id| !anthropic.contains(id))
            && anthropic.iter().any(|id| !plain.contains(id)),
        "each surface must own an id the other lacks: {plain:?} vs {anthropic:?}"
    );

    // A discovery probe through the Hub unions both surfaces, which is what the
    // profile the ignored tests create resolves against.
    //
    // The token matters: the Hub only sends `anthropic-version` when it has a
    // credential to send, so probing without one reads the plain surface alone
    // and the union would be missing every `claude-*` id.
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token.clone()).await?;
    // Armed only now: the direct probes above deliberately send no credential,
    // so an expectation in force earlier would have refused them.
    gateway.expect_credential(PROFILE_TOKEN);
    let discover = json!({
        "baseUrl": gateway.base_url(),
        "token": PROFILE_TOKEN,
    })
    .to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &[("Cookie", cookie.as_str())],
        Some(&discover),
    )
    .await?;
    anyhow::ensure!(status == 200, "discover {status} {rest}");
    let found: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(found["ok"], true, "{found}");
    // `/discover` answers its own shape: the catalog sits under `models`.
    let discovered: Vec<String> = found["models"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for id in plain.iter().chain(anthropic.iter()) {
        assert!(
            discovered.contains(id),
            "the union must offer {id}; got {discovered:?}"
        );
    }

    // The probe presented the profile credential and it compared equal — the
    // credential-swap check, not the no-credentials one (a probe that carries a
    // credential is correct, so `assert_no_credentials` is the wrong tool).
    assert_profile_credential_swapped(&gateway)?;

    gateway.shutdown().await;
    Ok(())
}

/// The ids of a `/v1/models` response body, in order.
fn model_ids(body: &Value) -> Result<Vec<String>> {
    Ok(body["data"]
        .as_array()
        .context("data")?
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_string))
        .collect())
}

/// A profile pointing at the fixture is accepted, and `POST /test` reaches it.
///
/// The ignored tests all need a stored profile whose `baseUrl` is the fixture;
/// this proves that part works today, independently of the router.
#[tokio::test]
async fn a_profile_can_point_at_the_fake_gateway_and_test_it() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token.clone()).await?;

    // The gateway will insist on the profile's token, so a probe that failed to
    // attach it would be refused rather than silently accepted.
    gateway.expect_credential(PROFILE_TOKEN);

    let id = create_profile(hub.addr, &cookie, &gateway.base_url()).await?;
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        &format!("/v1/providers/{id}/test"),
        &[("Cookie", cookie.as_str())],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "provider test {status} {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["reachable"], true, "{result}");

    // The stored profile's credential is what reached the origin.
    assert_profile_credential_swapped(&gateway)?;
    assert!(
        gateway.saw_header("anthropic-version"),
        "the probe reads the anthropic surface too: {:?}",
        gateway.header_names()
    );
    gateway.shutdown().await;
    Ok(())
}

// ── Ignored until `c-apiroute-hub` lands: the relay's own acceptance ───────

/// Happy path: one relayed Messages request, streamed back to the worker.
#[tokio::test]
async fn relays_a_messages_request_and_streams_the_response_back() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    // The origin insists on the profile credential, so a relay that forwarded
    // the worker's per-instance bearer is refused here rather than passing.
    gateway.expect_credential(PROFILE_TOKEN);
    // Explicit delivery configuration: the Hub itself is the proxy
    // (`apiVia: self`), so it egresses to the pinned origin.
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    // api.open is a notification: no id, no ack, and the response leg arrives
    // as notifications too.
    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_1", &messages_body()),
    )
    .await?;

    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_1") && frame["params"].get("bytesDown").is_some()
    })
    .await?;

    // The response head arrives before the body, so the worker's listener can
    // commit a status line without waiting for the first byte.
    let head = api_head(&frames, "st_1").context("no api.head")?;
    assert_eq!(head["status"], 200, "{head}");

    let body = String::from_utf8(reassemble_chunks(&frames_for(&frames, "st_1"))?)?;
    assert!(body.contains("event: message_start"), "{body}");
    assert!(body.contains("event: content_block_delta"), "{body}");
    assert!(body.contains("event: message_stop"), "{body}");
    assert_eq!(reassemble_deltas(&body)?, DEFAULT_TEXT);

    // ApiEndParams carries counters only — `{streamId, error?, bytesUp,
    // bytesDown, ms}`. No status, no body, no header ever reaches it, which is
    // what lets `api.end` be the journal record.
    let end = frames_for(&frames, "st_1")
        .into_iter()
        .find(|params| params.get("bytesDown").is_some())
        .context("no api.end")?;
    assert!(
        end.get("error").is_none(),
        "the stream ended in error: {end}"
    );
    assert!(
        end["bytesDown"].as_u64().unwrap_or(0) >= body.len() as u64,
        "api.end must count the response bytes: {end}"
    );
    assert!(
        end["bytesUp"].as_u64().is_some() && end["ms"].as_u64().is_some(),
        "api.end carries both counters and a duration: {end}"
    );
    assert!(
        end.get("status").is_none()
            && end.get("headers").is_none()
            && end.get("bodyBase64").is_none(),
        "api.end carries counters, never a status, body or header: {end}"
    );

    // The credential swap: the origin saw the profile credential, and the
    // worker's relay bearer never left the worker.
    //
    // The fixture is configured with the *profile* token and separately told
    // which value the worker holds, so it can distinguish the two by value. A
    // relay that forwarded the worker's bearer unchanged — the mistake this
    // whole design exists to prevent — presents a credential that is not the
    // configured one, is refused `401`, and reports a mismatch here.
    assert_profile_credential_swapped(&gateway)?;
    // The worker's own bearer is a different value, so if it were what arrived
    // the assertion above would already have failed; this makes the intent
    // explicit and keeps the constant load-bearing.
    assert_ne!(PROFILE_TOKEN, WORKER_RELAY_BEARER);
    assert!(
        gateway.saw_header("anthropic-version") && gateway.saw_header("x-stainless-lang"),
        "the request header allowlist carries these through: {:?}",
        gateway.header_names()
    );
    gateway.shutdown().await;
    Ok(())
}

/// A request body too large for one frame is chunked as `api.body`.
#[tokio::test]
async fn relays_a_chunked_request_body_larger_than_one_frame() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    // Self egress with an explicit route: the body travels over the in-band
    // link in api.body frames and the Hub reassembles it before egress.
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    // ≥400 KiB so it needs at least 6 slices — beyond the 4-credit listener
    // window; without the Hub returning api.credit for each consumed chunk the
    // later slices stall forever.
    let filler = "x".repeat(400 * 1024);
    let body = json!({
        "model": "fake/model-1",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": filler }],
    })
    .to_string();

    // Open with no inline body and `bodyChunked`, then push the body as
    // `api.body` notifications — one per slice, `last` on the final one.
    let mut params = api_open_params(&fixture.instance_id, "st_chunked", "");
    params["bodyBase64"] = Value::Null;
    params["bodyChunked"] = json!(true);
    notify(&mut fixture.node, "api.open", params).await?;

    // The worker socket receives api.credit frames as the Hub consumes each
    // api.body chunk; they sit in the socket and are counted after all slices
    // are sent (the yields give them time to land).
    let slices: Vec<&[u8]> = body.as_bytes().chunks(API_CHUNK_BYTES).collect();
    assert!(
        slices.len() >= 6,
        "a 400 KiB body needs at least 6 chunks, got {}",
        slices.len()
    );
    for (seq, slice) in slices.iter().enumerate() {
        let last = seq + 1 == slices.len();
        notify(
            &mut fixture.node,
            "api.body",
            api_body_params("st_chunked", seq, slice, last),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The origin received the whole body — the only thing that proves the
    // chunking was lossless rather than merely accepted frame by frame.
    let received = wait_for_origin(&gateway, &mut fixture.node, "st_chunked", |request| {
        request.body_bytes == body.len()
    })
    .await?;
    assert_eq!(
        received.body_bytes,
        body.len(),
        "the origin must see the reassembled body length"
    );

    // Count the api.credit frames the worker socket received for this stream.
    // Draining with a short budget collects what already landed (the credits
    // are emitted synchronously as each body chunk is consumed).
    let frames =
        collect_notifications(&mut fixture.node, Duration::from_millis(500), |_| false).await?;
    let credits: u32 = frames
        .iter()
        .filter(|frame| {
            frame["method"] == json!("api.credit")
                && frame["params"]["streamId"] == json!("st_chunked")
        })
        .map(|frame| frame["params"]["chunks"].as_u64().unwrap_or(0) as u32)
        .sum();
    assert!(
        credits >= slices.len() as u32,
        "the Hub must return an api.credit per consumed api.body chunk ({} slices, {credits} credits)",
        slices.len()
    );
    gateway.shutdown().await;
    Ok(())
}

/// H going offline mid-stream ends the stream truthfully — never by rerouting.
///
/// D-047 §Failure behaviour: falling back to direct delivery would leak the
/// request to a machine the operator excluded and make the session strip lie,
/// so the stream must end with an error and `remuda watch` must report
/// `api-route-down`.
#[tokio::test]
async fn a_via_host_lost_mid_stream_ends_the_stream_and_never_reroutes() -> Result<()> {
    // The origin serves the `via:<H>` case: when H drops mid-stream the Hub
    // itself never egresses, so the origin being empty at the end is the
    // "never rerouted" assertion. The route here is a dispatched worker with
    // an explicit `apiVia: H, hub-relay` delivery configuration.
    let gateway = FakeGateway::start().await?;
    let (mut fixture, mut proxy) = fixture_routed_via_h(&gateway.base_url_v1()).await?;
    let proxy_socket = proxy.socket();

    // D-048 (item 4): the credential reaches H out of band via api.egress,
    // which must arrive BEFORE the first api.open; the credential must never
    // appear in any api.open frame. Egress was already pushed at launch; now
    // open the stream and collect H's frames in wire order. Include a
    // worker-supplied credential header: the Hub must strip it before the
    // frame reaches H (the credential arrives only via api.egress).
    let mut open_params = api_open_params(&fixture.instance_id, "st_offline", &messages_body());
    open_params["headers"]
        .as_array_mut()
        .expect("headers array")
        .push(json!({ "name": "Authorization", "value": "Bearer worker-local-leak" }));
    open_params["headers"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "name": "x-api-key", "value": "worker-local-key" }));
    notify(&mut fixture.node, "api.open", open_params).await?;
    let (egress, open) = tokio::time::timeout(TIMEOUT, async {
        let mut egress: Option<Value> = None;
        loop {
            let frame = recv_json(proxy_socket).await?;
            match frame["method"].as_str() {
                Some("api.egress") if egress.is_none() => {
                    egress = Some(frame);
                }
                Some("api.open") => {
                    return Ok::<_, anyhow::Error>((
                        egress.context("api.egress must precede api.open")?,
                        frame,
                    ));
                }
                other => eprintln!("H saw other frame: {other:?}"),
            }
        }
    })
    .await??;
    assert_eq!(
        egress["params"]["authToken"].as_str(),
        Some(PROFILE_TOKEN),
        "api.egress carries the gateway credential: {egress}"
    );
    assert_eq!(
        egress["params"]["instanceId"].as_str(),
        Some(fixture.instance_id.as_str()),
        "api.egress is bound to the instance: {egress}"
    );
    assert!(
        open["params"].get("upstream").is_none(),
        "api.open must carry no upstream/credential snapshot: {open}"
    );
    assert!(
        open["params"].get("authToken").is_none(),
        "the credential must never ride api.open: {open}"
    );
    // The Hub must strip any worker-supplied credential headers before
    // forwarding to H, even if the listener let one through.
    let forwarded_auth_headers: Vec<&str> = open["params"]["headers"]
        .as_array()
        .map(|headers| {
            headers
                .iter()
                .filter_map(|h| h["name"].as_str())
                .filter(|name| {
                    matches!(
                        name.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key" | "api-key"
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        forwarded_auth_headers.is_empty(),
        "authorization/x-api-key/api-key must be stripped before api.open reaches H: \
         {forwarded_auth_headers:?} in {open}"
    );

    // H goes away with the stream in flight. The worker's socket stays open:
    // the terminal `api.end` for this stream has to arrive on it, and a test
    // that dropped it could never see the frame it is asserting about.
    proxy.drop_link(); // close H's actual socket

    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_offline")
            && frame["params"].get("bytesDown").is_some()
    })
    .await?;
    let end = frames_for(&frames, "st_offline")
        .into_iter()
        .find(|params| params.get("bytesDown").is_some())
        .context("the stream must be terminated, not abandoned")?;
    assert_eq!(
        end["error"]["code"], "via-host-offline",
        "the terminal frame must name the cause: {end}"
    );

    // No code path may reach the origin another way once mode == via. A direct
    // fallback would have to present a credential to the pinned origin, so
    // nothing may have arrived at all.
    assert!(
        gateway.requests().is_empty(),
        "a lost via host must not push the request anywhere else: {:?}",
        gateway.requests()
    );
    assert!(
        !gateway.saw_credential_mismatch(),
        "no fallback request may reach the origin with any credential"
    );

    // The roster observation becomes the reason `remuda watch` reports.
    let (status, _, rest) = http(
        fixture.addr,
        "POST",
        "/v1/workers/observe",
        &[("Cookie", fixture.cookie.as_str())],
        Some("{}"),
    )
    .await?;
    assert_eq!(status, 200, "observe {status} {rest}");
    let observed: Value = serde_json::from_str(rest.trim())?;
    // The blocked reason lands on the roster row's `state` (Blocked{reason})
    // and is also reflected on the watch block once a classify pass runs.
    let state_reasons: Vec<&str> = observed["items"]
        .as_array()
        .map(|workers| {
            workers
                .iter()
                .filter_map(|worker| {
                    worker
                        .pointer("/state/reason")
                        .and_then(Value::as_str)
                        .or_else(|| worker["reason"].as_str())
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        state_reasons.contains(&"api-route-down"),
        "a lost route is reported, not hidden: {observed}"
    );

    gateway.shutdown().await;
    Ok(())
}

/// A request-body frame over `apiChunkBytes` is refused, not truncated.
///
/// D-048 sets `apiChunkBytes` at 64 KiB raw (≈87 KiB base64), far under the
/// 1 MiB `maxJsonFrameBytes`. The oversized frame has to be sent on the leg
/// that actually carries request bodies — W Node→Hub, i.e. `api.body`. An
/// oversized `api.chunk` would be refused for *direction* (that method travels
/// H→Hub→W), which would prove nothing about the size limit.
#[tokio::test]
async fn an_oversized_api_body_frame_is_refused_not_truncated() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    let mut params = api_open_params(&fixture.instance_id, "st_big", "");
    params["bodyBase64"] = Value::Null;
    params["bodyChunked"] = json!(true);
    notify(&mut fixture.node, "api.open", params).await?;

    // Several times the cap in one frame.
    let oversized = vec![b'x'; API_CHUNK_BYTES * 4];
    notify(
        &mut fixture.node,
        "api.body",
        api_body_params("st_big", 0, &oversized, false),
    )
    .await?;

    // A notification cannot be answered with a JSON-RPC error, so the refusal
    // shows up the way §B.2 says it does: the stream is cancelled upstream and
    // terminated downstream with `api.end{error}`. What must *not* happen is
    // silent acceptance, and the origin must never see the oversized body.
    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_big") && frame["params"].get("bytesDown").is_some()
    })
    .await?;
    let end = frames_for(&frames, "st_big")
        .into_iter()
        .find(|params| params.get("bytesDown").is_some())
        .context("an oversized frame must terminate the stream, not be swallowed")?;
    assert!(
        end["error"].is_object(),
        "the terminal frame must carry the error code: {end}"
    );
    assert!(
        gateway
            .requests()
            .iter()
            .all(|request| request.body_bytes <= API_CHUNK_BYTES),
        "the origin must never receive an oversized body: {:?}",
        gateway.requests()
    );

    gateway.shutdown().await;
    Ok(())
}

/// A live relay stream must not starve Hub→Node control frames.
///
/// Risk 1 in the plan: the Hub→Node outbound queue is 32 slots shared with tty
/// frames, so `api.*` uses its own stream table with per-stream credits rather
/// than that queue or the RPC pending map. The probe is a genuine Hub→Node
/// control frame on the *shared* queue: attaching a follower makes the Hub
/// issue `tty.attach` to the Node (`crates/remuda-hub/src/ws.rs`, the follow
/// handler). If relay chunks were filling the queue, that attach would arrive
/// late — or not at all before a consumer's patience runs out.
///
/// The probe is deliberately a Hub→Node frame rather than a Node→Hub one: the
/// hazard is the outbound direction, so a test that measured an inbound method
/// would not exercise the queue this risk is about.
#[tokio::test]
async fn tty_attach_still_flows_while_a_relay_stream_is_hot() -> Result<()> {
    // A response large enough to keep chunks flowing for a while.
    let gateway = FakeGateway::start_with(vec![Script::messages("x".repeat(512 * 1024))]).await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    // Open the relay stream and leave it running — no credits are sent, so the
    // producer stays at its cap with the queue under pressure.
    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_hot", &messages_body()),
    )
    .await?;

    // Attaching a follower is what makes the Hub push `tty.attach` down to the
    // Node; whether it interleaves with the hot relay stream is the test.
    let mut req = format!(
        "ws://{}/v1/follow?instanceId={}",
        fixture.addr, fixture.instance_id
    )
    .into_client_request()?;
    req.headers_mut()
        .insert("Cookie", fixture.cookie.parse().unwrap());
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    let snapshot = recv_json(&mut follow).await?;
    anyhow::ensure!(snapshot["type"] == json!("snapshot"), "{snapshot}");
    follow
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "instanceIds": [fixture.instance_id],
                "tty": 1
            })
            .to_string()
            .into(),
        ))
        .await?;

    // Read the Node's socket until the Hub's `tty.attach` shows up, answering
    // relay notifications into the count so the producer is not blocked by a
    // full socket buffer.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut attached = false;
    let mut relay_chunks = 0usize;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), recv_json(&mut fixture.node)).await {
            Ok(Ok(frame)) => {
                if frame["method"] == json!("tty.attach") {
                    attached = true;
                    break;
                }
                if frame["method"] == json!("api.chunk") {
                    relay_chunks += 1;
                }
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => continue,
        }
    }
    assert!(
        attached,
        "the Hub must still issue tty.attach to the Node while a relay stream is hot"
    );
    assert!(
        relay_chunks >= 1,
        "the relay stream must actually be flowing for this to prove anything"
    );
    // Relative ordering: tty.attach must be observed while relay chunks are
    // still flowing (we broke out of the read loop on attach and counted at
    // least one chunk interleaved with it), not after the stream drained.
    // No absolute wall-clock ceiling — on a shared CI host a few seconds of
    // scheduling noise is not evidence of queue starvation; the interleaving
    // (chunk ≥1 before/around attach) is the property D-048 cares about.

    gateway.shutdown().await;
    Ok(())
}

/// D-048 §7.6 (round-3 item 6): the 50 ms coalesce flush is a timer raced
/// against the upstream read, not a check evaluated when the next byte
/// happens to arrive. A small SSE prologue followed by a multi-second idle
/// gap must reach the listener as an api.chunk well before the gap ends.
#[tokio::test]
async fn the_hub_egress_coalesces_and_delivers_incrementally_before_end() -> Result<()> {
    // D-048 §7.6: coalescing flushes at ≥16 KiB OR ≥50 ms. A small event
    // (far under 16 KiB) followed by a 2 s gap must arrive as an api.chunk
    // within ~50 ms, proving the timer fires while the egress is waiting for
    // the next upstream byte — not after the whole body is buffered.
    let gap_ms = 2_000u64;
    let gateway = FakeGateway::start_with(vec![Script::DelayedSse {
        gap_ms,
        text: "small".into(),
    }])
    .await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_incremental", &messages_body()),
    )
    .await?;

    // Wait only for the first data chunk. It must arrive well before the 2 s
    // gap ends (allow generous scheduling slack on a shared CI host).
    let started = std::time::Instant::now();
    let first = collect_notifications(&mut fixture.node, Duration::from_secs(5), |frame| {
        let params = &frame["params"];
        params["streamId"] == json!("st_incremental") && params.get("dataBase64").is_some()
    })
    .await?;
    assert!(!first.is_empty(), "first chunk must arrive");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(gap_ms),
        "the first small chunk must flush on the 50 ms timer, well before the \
         {gap_ms} ms gap ends; took {elapsed:?}"
    );

    // Two small events under 16 KiB each still arrive as separate frames
    // (the second after the gap); drain and wait for the terminal end.
    notify(
        &mut fixture.node,
        "api.credit",
        json!({"streamId": "st_incremental", "chunks": 256}),
    )
    .await?;
    let tail = collect_notifications(&mut fixture.node, Duration::from_secs(10), |frame| {
        frame["params"]["streamId"] == json!("st_incremental")
            && frame["params"].get("bytesDown").is_some()
    })
    .await?;
    let saw_end = frames_for(&tail, "st_incremental")
        .iter()
        .any(|params| params.get("bytesDown").is_some());
    assert!(saw_end, "the stream must end after draining: {tail:?}");
    gateway.shutdown().await;
    Ok(())
}

/// D-048 §7.6 (round-3 item 7): coalescing, not the 64 KiB drain. Drive the
/// egress with several tiny SSE writes — the whole body far under 16 KiB —
/// each separated by an idle gap, and assert the writes arrive as separate
/// non-terminal api.chunk frames, every one under the frame cap. A 200 KiB
/// body would only prove the 64 KiB splitter; this proves the 50 ms timer
/// flushes small buffered bytes repeatedly.
#[tokio::test]
async fn coalescing_separates_small_delayed_writes_into_sub_cap_frames() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::SseWithGaps {
        gap_ms: 250,
        // A handful of bytes per event: the entire body is under a KiB of SSE
        // framing, nowhere near the 16 KiB coalesce threshold.
        text: "ab".into(),
        events: 3,
    }])
    .await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_coalesce", &messages_body()),
    )
    .await?;
    // Three data chunks plus the terminal empty chunk fit the initial 4-credit
    // producer window, so no api.credit round trip is needed.

    // Collect until the terminal api.end.
    let frames = collect_notifications(&mut fixture.node, Duration::from_secs(10), |frame| {
        frame["params"]["streamId"] == json!("st_coalesce")
            && frame["params"].get("bytesDown").is_some()
    })
    .await?;
    let data_frames: Vec<&Value> = frames
        .iter()
        .filter(|frame| {
            frame["method"] == json!("api.chunk")
                && frame["params"]["streamId"] == json!("st_coalesce")
                // Data frames only: the empty last:true marker is not a flush.
                && frame["params"].get("last") != Some(&json!(true))
        })
        .collect();
    assert!(
        data_frames.len() >= 2,
        "several small delayed writes must flush as several frames, got {}: {frames:?}",
        data_frames.len()
    );
    for frame in &data_frames {
        let b64 = frame["params"]["dataBase64"].as_str().unwrap_or("");
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)?;
        assert!(
            bytes.len() < API_CHUNK_BYTES,
            "every timer-flushed frame must stay under the {API_CHUNK_BYTES} cap, got {}",
            bytes.len()
        );
    }
    let ended = frames_for(&frames, "st_coalesce")
        .iter()
        .any(|params| params.get("bytesDown").is_some());
    assert!(ended, "the stream must terminate: {frames:?}");
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_302_redirect_never_leaks_the_credential_to_the_target() -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    // A bare TCP listener that records every request and answers 200. It is
    // the redirect target; the gateway credential must never reach it.
    let target_hits = Arc::new(AtomicU32::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target_addr = listener.local_addr()?;
    let target_url = format!("http://{target_addr}/stolen");
    let target_hits_server = target_hits.clone();
    let target_task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            target_hits_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = vec![0; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await;
        }
    });

    // The gateway answers 302 → target.
    let gateway = FakeGateway::start_with(vec![remuda_testing::fake_gateway::Script::Redirect {
        location: target_url.clone(),
    }])
    .await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_redirect", &messages_body()),
    )
    .await?;

    // The relay does not follow the redirect: the stream ends with a refusal
    // (not a 200 from the target, not an upstream-timeout).
    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_redirect")
            && frame["params"].get("bytesDown").is_some()
    })
    .await?;
    let end = frames_for(&frames, "st_redirect")
        .into_iter()
        .find(|params| params.get("bytesDown").is_some())
        .context("redirect stream must terminate")?;
    assert!(
        end.get("error").is_some(),
        "a 302 must end the stream with an error, not a silent success: {end}"
    );
    assert_eq!(
        end["error"]["code"], "destination-refused",
        "a redirect is a destination the relay refuses to follow: {end}"
    );

    // Give the target a beat: it must have received zero connections.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        target_hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the redirect target must never be contacted; the credential would leak"
    );
    drop(target_task);
    gateway.shutdown().await;
    Ok(())
}

/// A destination outside the profile's pinned origin is refused.
///
/// Risk 5: the relay must not become a general HTTP proxy. The Node pins origin
/// and path; the Hub re-checks. A refused request must never reach the origin.
#[tokio::test]
async fn a_non_allowlisted_destination_is_refused() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    // A path escaping the profile's base path.
    let mut escaping = api_open_params(&fixture.instance_id, "st_evil1", &messages_body());
    escaping["path"] = json!("/v1/../../etc/passwd");
    notify(&mut fixture.node, "api.open", escaping).await?;

    // A request trying to name another origin.
    let mut elsewhere = api_open_params(&fixture.instance_id, "st_evil2", &messages_body());
    elsewhere["headers"] = json!([
        { "name": "content-type", "value": "application/json" },
        { "name": "x-evil-tunnel", "value": "attempt" }
    ]);
    notify(&mut fixture.node, "api.open", elsewhere).await?;

    // Both must be terminated with `destination-refused`, and the pinned origin
    // must see neither. The stop predicate waits for *both* ends: the two
    // refusal api.end frames are independent notifications and either can be
    // first on the socket.
    let mut got_evil1 = false;
    let mut got_evil2 = false;
    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        let params = &frame["params"];
        if params.get("bytesDown").is_none() {
            return false;
        }
        if params["streamId"] == json!("st_evil1") {
            got_evil1 = true;
        }
        if params["streamId"] == json!("st_evil2") {
            got_evil2 = true;
        }
        got_evil1 && got_evil2
    })
    .await?;
    for stream in ["st_evil1", "st_evil2"] {
        let end = frames_for(&frames, stream)
            .into_iter()
            .find(|params| params.get("bytesDown").is_some())
            .with_context(|| format!("{stream} must be terminated, not ignored"))?;
        assert_eq!(
            end["error"]["code"], "destination-refused",
            "{stream} must be refused by name: {end}"
        );
    }
    assert_eq!(
        gateway.request_count(),
        0,
        "a refused destination must never reach the gateway: {:?}",
        gateway.requests()
    );
    gateway.shutdown().await;
    Ok(())
}

/// Credit exhaustion stalls the producer instead of filling the shared queue.
///
/// D-048: a producer may have at most 4 unacked chunks per stream and stalls at
/// the cap; the consumer sends `api.credit` as it drains. Without this, a long
/// SSE stream would fill the 32-slot outbound queue and block tty frames.
#[tokio::test]
async fn a_producer_stalls_at_the_credit_cap() -> Result<()> {
    // A delayed-head SSE completion whose body sits just over one credit
    // window (5 × 64 KiB = 320 KiB ≈ 5 chunks): enough to prove the stall at
    // four *and* the resume on credit, small enough to finish promptly even
    // with the fixture building the document in memory.
    let gateway = FakeGateway::start_with(vec![Script::SlowFirstByte {
        delay: Duration::from_millis(300),
        text: "z".repeat(320 * 1024),
    }])
    .await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_credit", &messages_body()),
    )
    .await?;

    // Read whatever arrives without crediting, and count it.
    //
    // The stop predicate counts *cumulatively* through the closure's captured
    // state. The producer's hard window is 4 unacked chunks; once it has sent
    // four it blocks, so no fifth chunk can ever arrive. Stop as soon as a
    // short quiet window confirms the stream is parked.
    let mut seen_chunks = 0usize;
    let mut frames = collect_notifications(&mut fixture.node, Duration::from_secs(12), |frame| {
        if frame["params"]["streamId"] == json!("st_credit")
            && frame["params"].get("dataBase64").is_some()
        {
            seen_chunks += 1;
        }
        // Once the first chunk arrives the window is exercised; the cap test
        // only needs *some* chunks to have flowed before crediting.
        seen_chunks >= 1
    })
    .await?;
    // Keep draining (without crediting) for a quiet beat: the producer must
    // not exceed the window even given time to buffer.
    let more =
        collect_notifications(&mut fixture.node, Duration::from_millis(800), |_| false).await?;
    frames.extend(more);
    let uncredited = frames_for(&frames, "st_credit")
        .iter()
        .filter(|params| params.get("dataBase64").is_some())
        .count();

    // Both halves matter: at least one chunk must have arrived (the stream
    // really started) and no more than the cap must have (it really stalled).
    // Asserting only the upper bound would pass on a stream that never ran.
    assert!(
        uncredited >= 1,
        "the relay stream must start before it can stall at a cap (saw {uncredited})"
    );
    assert!(
        uncredited <= 4,
        "the producer must stall at the credit cap, saw {uncredited} uncredited chunks"
    );

    // Credit the stream and the producer resumes.
    notify(
        &mut fixture.node,
        "api.credit",
        json!({"streamId":"st_credit", "chunks": 4}),
    )
    .await?;
    let resumed = collect_notifications(&mut fixture.node, Duration::from_secs(10), |frame| {
        frame["params"]["streamId"] == json!("st_credit")
            && frame["params"].get("dataBase64").is_some()
    })
    .await?;
    assert!(
        frames_for(&resumed, "st_credit")
            .iter()
            .any(|params| params.get("dataBase64").is_some()),
        "the producer must resume after a credit"
    );

    gateway.shutdown().await;
    Ok(())
}

/// A `429` observed at the origin reaches supply evidence as a status.
///
/// Two halves, deliberately kept apart. On the **wire**, `ApiHeadParams` is
/// where a status travels, so the relayed `api.head` must carry `429` and must
/// carry `retry-after` through the response allowlist. `ApiEndParams` has no
/// status field at all — only `{streamId, error?, bytesUp, bytesDown, ms}` — so
/// asserting one there would be asserting on a field the wire does not have.
///
/// On the **record**, the design names supply evidence: H projects the observed
/// status into `POST /v1/providers/{id}/supply/events` (D-047 §Journal), and a
/// 429 is what cools a family window. The projection is the router's job; this
/// shows the surface it lands on, which is where a test must look for it.
#[tokio::test]
async fn an_origin_429_is_projected_into_supply_evidence() -> Result<()> {
    let gateway = FakeGateway::start_with(vec![Script::status(429)]).await?;
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;
    let profile_id = fixture_profile_id(&fixture).await?;

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_429", &messages_body()),
    )
    .await?;

    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_429") && frame["params"].get("bytesDown").is_some()
    })
    .await?;

    // On the wire: the status is relayed verbatim, so the worker's listener
    // answers a real Anthropic rate-limit error rather than a transport fault.
    let head = api_head(&frames, "st_429").context("no api.head")?;
    assert_eq!(head["status"], 429, "{head}");
    assert!(
        head_header_names(&head)
            .iter()
            .any(|name| name == "retry-after"),
        "retry-after survives the response allowlist: {head}"
    );

    // And api.end carries counters only — no status, because it has no such
    // field, and no body or header either.
    let end = frames_for(&frames, "st_429")
        .into_iter()
        .find(|params| params.get("bytesDown").is_some())
        .context("no api.end")?;
    assert!(
        end.get("status").is_none()
            && end.get("headers").is_none()
            && end.get("bodyBase64").is_none(),
        "api.end is counters only: {end}"
    );

    // On the record: this is the surface the projection writes to, and it is
    // where a 429 becomes visible (`supply.lastError` names the cooled
    // families — the design stores no raw status field).
    let event = json!({
        "type": "textual",
        "text": "API Error: 429 rate limited",
        "httpStatus": 429,
    })
    .to_string();
    let (status, _, rest) = http(
        fixture.addr,
        "POST",
        &format!("/v1/providers/{profile_id}/supply/events"),
        &[("Cookie", fixture.cookie.as_str())],
        Some(&event),
    )
    .await?;
    anyhow::ensure!(status == 200, "supply event {status} {rest}");
    let (status, _, rest) = http(
        fixture.addr,
        "GET",
        &format!("/v1/providers/{profile_id}/supply"),
        &[("Cookie", fixture.cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "get supply {status} {rest}");
    let supply: Value = serde_json::from_str(rest.trim())?;
    assert!(
        supply["supply"]["lastError"]
            .as_str()
            .is_some_and(|error| error.contains("429")),
        "the observed status must surface in the supply record: {supply}"
    );

    gateway.shutdown().await;
    Ok(())
}

/// The response allowlist drops `set-cookie` and keeps `content-type`.
#[tokio::test]
async fn the_response_header_allowlist_drops_set_cookie() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    // The origin must actually send a `set-cookie`, or this assertion would
    // pass whether or not the allowlist works.
    gateway.set_response_header("set-cookie", "session=should-never-be-relayed; HttpOnly");
    let mut fixture = fixture_routed_self(&gateway.base_url_v1()).await?;

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_hdr", &messages_body()),
    )
    .await?;

    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_hdr") && frame["params"].get("status").is_some()
    })
    .await?;
    let head = api_head(&frames, "st_hdr").context("no api.head")?;
    let names = head_header_names(&head);
    assert!(
        !names.iter().any(|name| name == "set-cookie"),
        "set-cookie is always dropped: {names:?}"
    );
    assert!(
        names.iter().any(|name| name == "content-type"),
        "content-type survives: {names:?}"
    );

    // The value must not be relayed either, not merely the name omitted.
    assert!(
        !head.to_string().contains("should-never-be-relayed"),
        "a dropped header's value must not appear anywhere on the wire: {head}"
    );
    gateway.shutdown().await;
    Ok(())
}

/// Wait for the origin to answer the first request matching `predicate`.
///
/// The relay's own progress is invisible from either socket (both legs are
/// notifications), so the origin's recorder is the honest place to observe that
/// a request completed — and it is the only place a test can see the body the
/// origin actually received.
async fn wait_for_origin<F>(
    gateway: &FakeGateway,
    node: &mut NodeSocket,
    stream_id: &str,
    mut predicate: F,
) -> Result<remuda_testing::fake_gateway::RecordedRequest>
where
    F: FnMut(&remuda_testing::fake_gateway::RecordedRequest) -> bool,
{
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Some(request) = gateway.requests().into_iter().find(|req| predicate(req)) {
            return Ok(request);
        }
        // Drain the worker's socket so a producer blocked on credits can make
        // progress; ignore whatever comes back.
        let _ = collect_notifications(node, Duration::from_millis(200), |frame| {
            frame["params"]["streamId"] == json!(stream_id)
                && frame["params"].get("bytesDown").is_some()
        })
        .await?;
    }
    Err(anyhow!(
        "the relayed request never reached the origin for {stream_id}"
    ))
}

/// Item 1: the egress context is per *instance*, not per stream — two
/// sequential api.open/api.end cycles on one via instance must both get a
/// normal proxied response. The Hub must not revoke after the first stream.
#[tokio::test]
async fn two_sequential_streams_on_one_via_instance_both_egress() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let (mut fixture, mut proxy) = fixture_routed_via_h(&gateway.base_url_v1()).await?;
    let proxy_host = proxy.host_id.clone();
    let proxy_socket = proxy.socket();

    for cycle in 0..2 {
        let stream_id = format!("st_cycle_{cycle}");
        notify(
            &mut fixture.node,
            "api.open",
            api_open_params(&fixture.instance_id, &stream_id, &messages_body()),
        )
        .await?;

        // H receives api.egress (only the first time; the second is a no-op
        // re-send if it happens, but the context must still be present) then
        // api.open addressed to H.
        let open = tokio::time::timeout(TIMEOUT, async {
            loop {
                let frame = recv_json(proxy_socket).await?;
                if frame["method"] == json!("api.open") {
                    return Ok::<_, anyhow::Error>(frame);
                }
            }
        })
        .await??;
        let h_stream = open["params"]["streamId"]
            .as_str()
            .with_context(|| format!("cycle {cycle}: api.open missing streamId"))?;
        assert!(
            h_stream.starts_with(&stream_id),
            "cycle {cycle}: H's stream {h_stream} must be derived from {stream_id}"
        );

        // Simulate H's response: head, one chunk, end.
        send_json(
            proxy_socket,
            json!({
                "jsonrpc": "2.0", "method": "api.head",
                "params": {"streamId": open["params"]["streamId"], "status": 200, "headers": []}
            }),
        )
        .await?;
        send_json(
            proxy_socket,
            json!({
                "jsonrpc": "2.0", "method": "api.chunk",
                "params": {
                    "streamId": open["params"]["streamId"],
                    "seq": 0,
                    "dataBase64": base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        b"ok",
                    ),
                    "last": false,
                }
            }),
        )
        .await?;
        send_json(
            proxy_socket,
            json!({
                "jsonrpc": "2.0", "method": "api.end",
                "params": {
                    "streamId": open["params"]["streamId"],
                    "bytesUp": 10, "bytesDown": 2, "ms": 5,
                }
            }),
        )
        .await?;

        // W sees the terminal end for this stream — both cycles.
        let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
            frame["params"]["streamId"] == json!(stream_id)
                && frame["params"].get("bytesDown").is_some()
        })
        .await?;
        let saw_end = frames_for(&frames, &stream_id)
            .iter()
            .any(|params| params.get("bytesDown").is_some());
        assert!(
            saw_end,
            "cycle {cycle}: stream {stream_id} must end normally"
        );
    }
    let _ = proxy_host;
    gateway.shutdown().await;
    Ok(())
}

/// Round-4 item 1: a normal session end arrives as a lifecycle journal event
/// (`exited`), with no api.* frame on the path. The Hub must revoke the
/// instance's egress context from H then — otherwise the gateway credential
/// stays installed for the life of H's link.
#[tokio::test]
async fn an_exited_lifecycle_journal_revokes_egress_on_proxy() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let (mut fixture, mut proxy) = fixture_routed_via_h(&gateway.base_url_v1()).await?;
    let proxy_socket = proxy.socket();

    // The install frame was pushed at dispatch; consume it so the revoke
    // frame is the next egress observed on H's socket.
    let install = tokio::time::timeout(TIMEOUT, async {
        loop {
            let frame = recv_json(proxy_socket).await?;
            if frame["method"] == json!("api.egress")
                && frame["params"]["instanceId"] == json!(fixture.instance_id)
            {
                return Ok::<_, anyhow::Error>(frame);
            }
        }
    })
    .await??;
    assert!(
        install["params"].get("revoke") != Some(&json!(true)),
        "the launch-time frame installs, not revokes: {install}"
    );

    // W journals the instance exiting normally.
    let (ack, _) = request(
        &mut fixture.node,
        json!({
            "jsonrpc": "2.0", "id": "exit-1", "method": "journal.append",
            "params": {
                "instanceId": fixture.instance_id,
                "event": {
                    "kind": "lifecycle",
                    "payload": {
                        "type": "entity", "entityType": "instance",
                        "state": "exited", "reasonCode": "session-ended"
                    }
                }
            }
        }),
        |_| false,
    )
    .await?;
    assert!(ack.get("result").is_some(), "journal append: {ack}");

    // H must receive revoke:true for this instance.
    let revoke = tokio::time::timeout(TIMEOUT, async {
        loop {
            let frame = recv_json(proxy_socket).await?;
            if frame["method"] == json!("api.egress")
                && frame["params"]["instanceId"] == json!(fixture.instance_id)
                && frame["params"]["revoke"] == json!(true)
            {
                return Ok::<_, anyhow::Error>(frame);
            }
        }
    })
    .await??;
    assert_eq!(revoke["params"]["revoke"], json!(true), "{revoke}");
    assert_eq!(
        revoke["params"].get("authToken"),
        None,
        "a revoke never carries the credential: {revoke}"
    );
    gateway.shutdown().await;
    Ok(())
}

/// Item 3: the egress context is installed for **every** resolved via route,
/// not only an explicit hub-relay one. With `apiRoute: auto` against a host
/// that advertises a relayBind the request keeps sub-mode auto at launch; the
/// Node then falls back to hub-relay (the observation echoed here) and sends
/// api.open — the Hub must already hold the credential and answer normally.
/// The old code installed no context on this path, so the fallback open was
/// refused with "no egress context installed".
#[tokio::test]
async fn an_auto_route_against_a_relaybind_host_still_gets_egress() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let (mut fixture, mut proxy) = fixture_routed_via_h_with(
        &gateway.base_url_v1(),
        ViaOpts {
            route: "auto",
            relay_bind: true,
        },
    )
    .await?;
    let proxy_socket = proxy.socket();

    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_auto", &messages_body()),
    )
    .await?;

    // Egress may have landed at dispatch time or just now; either way it must
    // be on the socket BEFORE the forwarded api.open.
    let (egress, open) = tokio::time::timeout(TIMEOUT, async {
        let mut egress: Option<Value> = None;
        loop {
            let frame = recv_json(proxy_socket).await?;
            match frame["method"].as_str() {
                Some("api.egress")
                    if frame["params"]["instanceId"] == json!(fixture.instance_id) =>
                {
                    egress = Some(frame);
                }
                Some("api.open") => {
                    return Ok::<_, anyhow::Error>((
                        egress.context("auto route: api.egress must precede api.open")?,
                        frame,
                    ));
                }
                _ => {}
            }
        }
    })
    .await??;
    assert_eq!(
        egress["params"]["authToken"].as_str(),
        Some(PROFILE_TOKEN),
        "auto route still installs the credential out of band: {egress}"
    );
    assert!(
        open["params"]["streamId"]
            .as_str()
            .is_some_and(|id| id.starts_with("st_auto")),
        "api.open for st_auto must reach H: {open}"
    );

    // H answers the fallback stream normally.
    send_json(
        proxy_socket,
        json!({
            "jsonrpc": "2.0", "method": "api.head",
            "params": {"streamId": open["params"]["streamId"], "status": 200, "headers": []}
        }),
    )
    .await?;
    send_json(
        proxy_socket,
        json!({
            "jsonrpc": "2.0", "method": "api.end",
            "params": {"streamId": open["params"]["streamId"], "bytesUp": 10, "bytesDown": 0, "ms": 3}
        }),
    )
    .await?;

    let frames = collect_notifications(&mut fixture.node, TIMEOUT, |frame| {
        frame["params"]["streamId"] == json!("st_auto")
            && frame["params"].get("bytesDown").is_some()
    })
    .await?;
    let end = frames_for(&frames, "st_auto")
        .into_iter()
        .find(|p| p.get("bytesDown").is_some())
        .context("st_auto must terminate")?;
    assert!(
        end.get("error").is_none(),
        "the hub-relay fallback of an auto route must end normally, not refused: {end}"
    );
    gateway.shutdown().await;
    Ok(())
}
/// Item 4: a proxy host that drops and re-hellos receives a fresh api.egress
/// before the next api.open reaches it.
#[tokio::test]
async fn proxy_reconnect_gets_fresh_api_egress_before_next_open() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let (mut fixture, mut proxy) = fixture_routed_via_h(&gateway.base_url_v1()).await?;
    let proxy_host = proxy.host_id.clone();

    // H's first socket goes away (simulating a drop with no live stream).
    proxy.drop_link();
    wait_proxy_host_offline(&fixture, &proxy_host).await?;

    // Re-hello H on a fresh socket using its durable node token.
    proxy.reconnect(fixture.addr, "relay-proxy").await?;
    // Frames buffered ahead of the reconnect hello reply (e.g. api.egress).
    let mut buffered = proxy.take_pending();
    let reconnected = proxy.socket();

    // W opens a stream. The Hub must push api.egress to the new socket BEFORE
    // forwarding api.open. Accept an egress that arrived ahead of the hello
    // reply as well as one that arrives after.
    notify(
        &mut fixture.node,
        "api.open",
        api_open_params(&fixture.instance_id, "st_reconnect", &messages_body()),
    )
    .await?;
    let has_egress = |frame: &Value| {
        frame["method"] == json!("api.egress")
            && frame["params"]["instanceId"] == json!(fixture.instance_id)
    };
    let egress_pre = buffered.iter().find(|f| has_egress(f)).cloned();
    let (egress, open) = tokio::time::timeout(TIMEOUT, async move {
        if let Some(egress) = egress_pre {
            // Drain until api.open.
            loop {
                let frame = recv_json(reconnected).await?;
                if frame["method"] == json!("api.open") {
                    return Ok::<_, anyhow::Error>((egress, frame));
                }
                buffered.push(frame);
            }
        }
        // No buffered egress: wait for one, then for api.open.
        let mut egress: Option<Value> = None;
        while egress.is_none() {
            let frame = recv_json(reconnected).await?;
            if frame["method"] == json!("api.egress")
                && frame["params"]["instanceId"] == json!(fixture.instance_id)
            {
                egress = Some(frame);
            } else if frame["method"] == json!("api.open") {
                anyhow::bail!("api.open arrived before api.egress after reconnect");
            }
            // other frames: ignore and keep waiting
        }
        let egress = egress.unwrap();
        loop {
            let frame = recv_json(reconnected).await?;
            if frame["method"] == json!("api.open") {
                return Ok((egress, frame));
            }
        }
    })
    .await??;
    assert_eq!(
        egress["params"]["authToken"].as_str(),
        Some(PROFILE_TOKEN),
        "reconnected H must re-receive the credential"
    );
    assert!(
        open["params"]["streamId"]
            .as_str()
            .is_some_and(|id| id.starts_with("st_reconnect")),
        "api.open for st_reconnect must reach H: {open}"
    );
    gateway.shutdown().await;
    Ok(())
}

/// Round-4 item 2: the reconnect re-send goes through the SecretBroker host
/// scope gate like every other release path. A profile narrowed to *another*
/// host after launch must not be re-delivered to the proxy when it
/// reconnects — the Hub drops the stored snapshot and sends revoke instead.
#[tokio::test]
async fn a_rescoped_profile_is_not_redelivered_on_proxy_reconnect() -> Result<()> {
    let gateway = FakeGateway::start().await?;
    let (fixture, mut proxy) = fixture_routed_via_h(&gateway.base_url_v1()).await?;

    // An unrelated third host becomes the profile's sole release target.
    let (_scope_node, scope_host, _scope_ws, _scope_token) =
        connect_capable_node(fixture.addr, &fixture._hub, "scope-owner", false).await?;

    // The single gateway profile the fixture created.
    let (status, _, rest) = http(
        fixture.addr,
        "GET",
        "/v1/providers",
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "list providers {status} {rest}");
    let listed: Value = serde_json::from_str(rest.trim())?;
    let profile_id = listed["items"]
        .as_array()
        .context("items")?
        .iter()
        .find(|p| p["kind"] == json!("gateway"))
        .and_then(|p| p["id"].as_str())
        .context("gateway profile id")?
        .to_string();

    // Narrow the scope after launch: H is no longer allowed the secret.
    let (status, _, rest) = http(
        fixture.addr,
        "PATCH",
        &format!("/v1/providers/{profile_id}"),
        &[("Cookie", &fixture.cookie)],
        Some(&json!({ "scope": format!("host:{scope_host}") }).to_string()),
    )
    .await?;
    anyhow::ensure!(status == 200, "scope patch {status} {rest}");

    // H reconnects; the Hub re-runs egress installs for its routed instances.
    proxy.reconnect(fixture.addr, "relay-proxy").await?;
    let mut observed = proxy.take_pending();
    observed.extend(
        collect_notifications(proxy.socket(), Duration::from_millis(800), |_| false).await?,
    );

    // No install (secret-bearing) egress for the instance on this link.
    let installs: Vec<&Value> = observed
        .iter()
        .filter(|frame| {
            frame["method"] == json!("api.egress")
                && frame["params"]["instanceId"] == json!(fixture.instance_id)
                && frame["params"].get("revoke") != Some(&json!(true))
        })
        .collect();
    assert!(
        installs.is_empty(),
        "the rescoped secret must not be re-delivered to H: {installs:?}"
    );

    // The previously installed snapshot is revoked, credential-free.
    let revoke = observed
        .iter()
        .find(|frame| {
            frame["method"] == json!("api.egress")
                && frame["params"]["instanceId"] == json!(fixture.instance_id)
                && frame["params"]["revoke"] == json!(true)
        })
        .context("a rescoped profile must revoke the stale snapshot on reconnect")?;
    assert_eq!(
        revoke["params"].get("authToken"),
        None,
        "a revoke never carries the credential: {revoke}"
    );
    gateway.shutdown().await;
    Ok(())
}

