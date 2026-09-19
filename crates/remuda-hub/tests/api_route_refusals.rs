//! D-047 §B.5 refusal tests through the HTTP layer.
//!
//! `api-via-unknown-host` is a 400; `api-via-host-offline` /
//! `api-via-unsupported` / `api-via-unreachable` are 409. Every case is a
//! hard refusal before any name/port/worktree allocation, and no case ever
//! falls back to direct delivery.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    token: String,
}

async fn boot() -> Result<Ctx> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let token = hub.mint_device_token("refusal-test").await?;
    Ok(Ctx {
        _dir: dir,
        hub,
        http: reqwest::Client::new(),
        token,
    })
}

/// Connect a fake Node. `api_relay` controls the D-048 capability flag.
/// Like [`connect_node`] but close the socket immediately after hello: the
/// host stays enrolled while its link is offline.
async fn connect_offline_node(
    hub: &remuda_hub::RunningHub,
) -> Result<(HostId, remuda_protocol::WorkspaceId)> {
    let pair = connect_node_raw(hub, false, true).await?;
    Ok((pair.0, pair.1))
}

async fn connect_node(
    hub: &remuda_hub::RunningHub,
    api_relay: bool,
) -> Result<(HostId, remuda_protocol::WorkspaceId)> {
    connect_node_raw(hub, api_relay, false).await
}

async fn connect_node_raw(
    hub: &remuda_hub::RunningHub,
    api_relay: bool,
    close_after_hello: bool,
) -> Result<(HostId, remuda_protocol::WorkspaceId)> {
    let host_id = HostId::new();
    let workspace_id = remuda_protocol::WorkspaceId::new();
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (node, _) = tokio_tungstenite::connect_async(request).await?;
    let mut node: NodeSocket = node;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id(),
                "nodeVersion": if api_relay { "0.2.0-d048" } else { "0.1.0-old" },
                "label": "fake",
                "host": {
                    "maxInstances": 8,
                    "workspaces": [{
                        "workspaceId": workspace_id.as_id(),
                        "hostId": host_id.as_id(),
                        "root": "/tmp/wsp_test"
                    }],
                    "workspaceRevision": 1,
                },
                "capabilities": if api_relay {
                    json!({ "apiRelay": true })
                } else {
                    json!({})
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let frame = recv_json(&mut node).await?;
    anyhow::ensure!(frame.get("result").is_some(), "hello failed: {frame}");
    if close_after_hello {
        // Close frame then drop, so the Hub marks this enrolled host offline.
        let _ = node.send(Message::Close(None)).await;
        drop(node);
        return Ok((host_id, workspace_id));
    }
    tokio::spawn(async move {
        while let Some(Ok(message)) = node.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(id) = frame.get("id") {
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                let params = frame.get("params").cloned().unwrap_or(json!({}));
                let result = match method {
                    "worker.provision" => json!({
                        "name": "w",
                        "branch": "wt/w/work",
                        "startPoint": "origin/main",
                        "worktreePath": "/tmp/remuda-wt/w",
                        "targetDir": "/tmp/remuda-target/w"
                    }),
                    "worker.remove" => json!({ "worktreeRemoved": true, "targetRemoved": true }),
                    "instance.create" => {
                        let mut result = json!({ "accepted": true });
                        if let Some(route) = params.pointer("/spec/apiRoute").cloned() {
                            result["apiRoute"] = route;
                        }
                        result
                    }
                    _ => json!({ "ok": true }),
                };
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                if node2_send(&mut node, reply).await.is_err() {
                    break;
                }
            }
        }
    });
    Ok((host_id, workspace_id))
}

async fn node2_send(
    node: &mut NodeSocket,
    reply: Value,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    node.send(Message::Text(reply.to_string().into())).await
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

impl Ctx {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.hub.addr)
    }

    async fn post(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(json!(null));
        (status, body)
    }

    async fn patch(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let response = self
            .http
            .request(reqwest::Method::PATCH, self.url(path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(json!(null));
        (status, body)
    }

    async fn create_gateway(&self, delivery: Option<Value>) -> Result<String> {
        let mut body = json!({
            "name": "routed",
            "kind": "gateway",
            "baseUrl": "http://127.0.0.1:1",
            "models": ["passthrough/auto"],
            "authToken": "sk-fake-zzzz",
            "defaultGateway": true
        });
        if let Some(delivery) = delivery
            && let Some(dst) = body.as_object_mut()
        {
            dst.insert("delivery".into(), delivery);
        }
        let (status, value) = self.post("/v1/providers", body).await;
        assert!(status.is_success(), "create provider {status}: {value}");
        Ok(value["id"].as_str().context("id")?.to_string())
    }

    async fn create_project(&self, host_id: &str, workspace_id: &str) -> Result<String> {
        let (status, project) = self.post(
            "/v1/projects",
            json!({
                "name": "refusals",
                "members": [{ "hostId": host_id, "workspaceId": workspace_id, "role": "build" }],
                "hosts": [{ "hostId": host_id, "maxInstances": 8, "latencyClass": "remote" }]
            }),
        )
        .await;
        assert!(status.is_success(), "project {status}: {project}");
        Ok(project["id"].as_str().context("project id")?.to_string())
    }

    fn dispatch(&self, project_id: &str, extra: Value) -> Value {
        let mut body = json!({
            "projectId": project_id,
            "brief": "x",
            "harness": "claude",
            // claude-print needs no herdr/native shell, keeping these tests
            // about routing rather than carrier capability.
            "driver": "claude-print",
        });
        if let (Some(dst), Some(src)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in src {
                dst.insert(key.clone(), value.clone());
            }
        }
        body
    }
}

#[tokio::test]
async fn unknown_via_host_is_a_400_on_dispatch_and_create() -> Result<()> {
    let ctx = boot().await?;
    ctx.create_gateway(None).await?;
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;

    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": HostId::new().as_id().to_string() }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert_eq!(status, 400, "{value}");
    assert_eq!(value["code"], json!("api-via-unknown-host"), "{value}");

    // Same on the plain create surface.
    let (status, value) = ctx
        .post(
            "/v1/instances",
            json!({
                "hostId": worker.as_id(),
                "kind": "claude",
                "driver": "claude-print",
                "delegation": "gateway",
                "apiVia": HostId::new().as_id().to_string()
            }),
        )
        .await;
    assert_eq!(status, 400, "{value}");
    assert_eq!(value["code"], json!("api-via-unknown-host"), "{value}");
    Ok(())
}

#[tokio::test]
async fn offline_via_host_is_a_409_before_allocation() -> Result<()> {
    let ctx = boot().await?;
    ctx.create_gateway(None).await?;
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    // Enrolled but currently disconnected.
    let (offline_proxy, _offline_workspace) = connect_offline_node(&ctx.hub).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;

    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": offline_proxy.as_id().to_string(), "apiRoute": "hub-relay" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert_eq!(status, 409, "{value}");
    assert_eq!(value["code"], json!("api-via-host-offline"), "{value}");
    Ok(())
}

#[tokio::test]
async fn old_worker_and_old_proxy_nodes_are_409_unsupported() -> Result<()> {
    let ctx = boot().await?;
    ctx.create_gateway(None).await?;
    // The worker is old (no D-048 capability).
    let (old_worker, workspace) = connect_node(&ctx.hub, false).await?;
    let project_id = ctx
        .create_project(old_worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;
    let body = ctx.dispatch(&project_id, json!({ "apiVia": "self" }));
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert_eq!(status, 409, "{value}");
    assert_eq!(value["code"], json!("api-via-unsupported"), "{value}");

    // Worker capable, proxy old.
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    let (old_proxy, _) = connect_node(&ctx.hub, false).await?;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;
    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": old_proxy.as_id().to_string(), "apiRoute": "hub-relay" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert_eq!(status, 409, "{value}");
    assert_eq!(value["code"], json!("api-via-unsupported"), "{value}");
    Ok(())
}

#[tokio::test]
async fn direct_net_without_a_relay_bind_is_409_and_auto_falls_back_to_hub_relay() -> Result<()> {
    let ctx = boot().await?;
    ctx.create_gateway(None).await?;
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    let (proxy, _) = connect_node(&ctx.hub, true).await?;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;

    // Explicit direct-net, no relayBind configured: unreachable.
    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": proxy.as_id().to_string(), "apiRoute": "direct-net" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert_eq!(status, 409, "{value}");
    assert_eq!(value["code"], json!("api-via-unreachable"), "{value}");

    // direct-net to `self` is unconditionally unreachable.
    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": "self", "apiRoute": "direct-net" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert_eq!(status, 409, "{value}");
    assert_eq!(value["code"], json!("api-via-unreachable"), "{value}");

    // auto to a host with no bind is *decided* as hub-relay and launches.
    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": proxy.as_id().to_string(), "apiRoute": "auto" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert!(
        status.is_success(),
        "auto without a bind launches: {status} {value}"
    );
    Ok(())
}

#[tokio::test]
async fn direct_net_is_allowed_once_a_loopback_private_relay_bind_is_set() -> Result<()> {
    let ctx = boot().await?;
    ctx.create_gateway(None).await?;
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    let (proxy, _) = connect_node(&ctx.hub, true).await?;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;

    // Wildcards and public addresses are rejected on the PATCH.
    let (status, value) = ctx
        .patch(
            &format!("/v1/hosts/{}", proxy.as_id()),
            json!({ "relayBind": { "addr": "0.0.0.0:8443", "allowFrom": [] } }),
        )
        .await;
    assert_eq!(status, 400, "{value}");
    let (status, _) = ctx
        .patch(
            &format!("/v1/hosts/{}", proxy.as_id()),
            json!({ "relayBind": { "addr": "8.8.8.8:8443", "allowFrom": [] } }),
        )
        .await;
    assert_eq!(status, 400);

    // A private bind unlocks direct-net.
    let (status, value) = ctx
        .patch(
            &format!("/v1/hosts/{}", proxy.as_id()),
            json!({ "relayBind": { "addr": "10.0.0.2:8443", "allowFrom": ["10.0.0.0/8"] } }),
        )
        .await;
    assert!(status.is_success(), "relayBind patch: {status} {value}");
    assert_eq!(
        value["relayBind"]["addr"],
        json!("10.0.0.2:8443"),
        "relayBind reads back: {value}"
    );

    let body = ctx.dispatch(
        &project_id,
        json!({ "apiVia": proxy.as_id().to_string(), "apiRoute": "direct-net" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert!(
        status.is_success(),
        "direct-net with a bind launches: {status} {value}"
    );
    Ok(())
}

#[tokio::test]
async fn profile_delivery_via_is_resolved_and_none_forces_direct() -> Result<()> {
    let ctx = boot().await?;
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    let (proxy, _) = connect_node(&ctx.hub, true).await?;
    // Profile says via → proxy, hub-relay.
    ctx.create_gateway(Some(json!({
        "mode": "via",
        "viaHostId": proxy.as_id(),
        "route": "hub-relay"
    })))
    .await?;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;

    // No explicit override: the profile delivery applies and launches via.
    let body = ctx.dispatch(&project_id, json!({ "name": "via-worker" }));
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert!(
        status.is_success(),
        "profile via launches: {status} {value}"
    );

    // apiVia:none forces direct over the profile (distinct worker name).
    let body = ctx.dispatch(
        &project_id,
        json!({ "name": "direct-worker", "apiVia": "none" }),
    );
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert!(status.is_success(), "none forces direct: {status} {value}");
    Ok(())
}

#[tokio::test]
async fn via_the_worker_host_collapses_to_direct() -> Result<()> {
    let ctx = boot().await?;
    let (worker, workspace) = connect_node(&ctx.hub, true).await?;
    ctx.create_gateway(Some(json!({
        "mode": "via",
        "viaHostId": worker.as_id(),
        "route": "hub-relay"
    })))
    .await?;
    let project_id = ctx
        .create_project(worker.as_id().as_str(), workspace.as_id().as_str())
        .await?;
    let body = ctx.dispatch(&project_id, json!({}));
    let (status, value) = ctx.post("/v1/workers/dispatch", body).await;
    assert!(
        status.is_success(),
        "naming the worker itself is direct, not a refusal: {status} {value}"
    );
    Ok(())
}
