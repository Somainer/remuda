//! Placement must not refuse a host on a stale CPU sample.
//!
//! Covers the four freshness behaviors end to end with a fake Node over the
//! real `/v1/node` WebSocket:
//! 1. a stale saturated sample triggers a bounded `host.resources` refresh and
//!    the fresh low reading admits;
//! 2. a stale sample that cannot be refreshed does not refuse (never 422 on a
//!    number nobody could re-confirm);
//! 3. a fresh saturated sample still refuses auto-placement;
//! 4. an explicit hostId pin over a fresh saturated sample admits with a
//!    warning on the response and a journaled Hub diagnostic; and
//! 5. `GET /v1/hosts` carries `resources.sampledAt`.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

/// Freshness window / refresh timeout shortened so staleness takes
/// milliseconds to reach instead of a minute.
async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.resource_sample_max_age_ms = 250;
    config.resource_refresh_timeout_ms = 300;
    let hub = spawn(config).await?;
    let token = hub.bootstrap_token.clone();
    Ok((hub, token, dir))
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "freshness-test" }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    cookie_from(&head).context("set-cookie")
}

async fn enroll_token(addr: std::net::SocketAddr, cookie: &str) -> Result<String> {
    let (status, _, rest) = http(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", cookie)],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "enroll-token {status} {rest}");
    serde_json::from_str::<Value>(rest.trim())?["token"]
        .as_str()
        .map(str::to_string)
        .context("enroll token")
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match msg {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            other => return Err(anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

type NodeWs = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<TcpStream>,
>;

async fn connect_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
    resources: Value,
) -> Result<NodeWs> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": "freshness-node",
                "host": {
                    "maxInstances": 4,
                    "cli": [{ "kind": "claude", "version": "2.1.268", "path": "/usr/bin/claude", "auth": "unknown" }],
                    "resources": resources
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["hostId"] == host_id, "{hello}");
    Ok(node)
}

/// Answer every inbound `host.resources` RPC with `reply`; `None` models a
/// wedged/old Node that never responds (the Hub must time out and proceed).
fn serve_node(node: NodeWs, reply: Option<Value>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut node = node;
        while let Ok(Some(Ok(frame))) = tokio::time::timeout(TIMEOUT, node.next()).await {
            let Message::Text(text) = frame else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let (Some(method), Some(id)) = (
                value.get("method").and_then(Value::as_str),
                value.get("id").cloned(),
            ) else {
                continue;
            };
            if method == "host.resources"
                && let Some(resources) = &reply
            {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "resources": resources }
                });
                if node.send(Message::Text(response.to_string().into())).await.is_err() {
                    break;
                }
            }
            // instance.create and everything else is intentionally unanswered:
            // placement decides before the queued command settles.
        }
    })
}

async fn create(
    addr: std::net::SocketAddr,
    cookie: &str,
    body: Value,
) -> Result<(u16, Value)> {
    let (status, _, rest) = http(
        addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie)],
        Some(&body.to_string()),
    )
    .await?;
    Ok((status, serde_json::from_str(rest.trim())?))
}

#[tokio::test]
async fn hosts_carry_sampled_at_stamped_by_the_hub() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let node = connect_node(
        hub.addr,
        &enroll_token(hub.addr, &cookie).await?,
        &host_id,
        json!({ "cpuPct": 100, "memPct": 40, "cpuCount": 14 }),
    )
    .await?;
    let _node = serve_node(node, Some(json!({ "cpuPct": 4, "memPct": 30 })));

    let (status, _, body) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let hosts = serde_json::from_str::<Value>(body.trim())?;
    let resources = &hosts["items"][0]["resources"];
    assert_eq!(resources["cpuPct"], json!(100));
    assert!(
        resources["sampledAt"].as_str().is_some_and(|s| s.contains('T')),
        "sampledAt must be a stamped RFC3339 time: {resources}"
    );
    Ok(())
}

#[tokio::test]
async fn stale_saturated_sample_refreshes_and_auto_placement_admits() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let node = connect_node(
        hub.addr,
        &enroll_token(hub.addr, &cookie).await?,
        &host_id,
        json!({ "cpuPct": 100, "memPct": 40 }),
    )
    .await?;
    let _node = serve_node(node, Some(json!({ "cpuPct": 6, "memPct": 41, "cpuCount": 14 })));

    // Age the 100% sample beyond the 250 ms freshness window.
    tokio::time::sleep(Duration::from_millis(350)).await;

    let body = json!({
        "placement": { "kind": "any" },
        "driver": "claude-print",
        "delegation": "none"
    });
    let (status, response) = create(hub.addr, &cookie, body).await?;
    assert_eq!(status, 200, "fresh low load admits: {response}");
    assert_eq!(response["hostId"], json!(host_id));

    // The fresh reading is persisted for the next decision, no restart needed.
    let (_, _, hosts_body) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    let resources = &serde_json::from_str::<Value>(&hosts_body)?["items"][0]["resources"];
    assert_eq!(resources["cpuPct"], json!(6), "{resources}");
    Ok(())
}

#[tokio::test]
async fn stale_sample_that_cannot_be_refreshed_does_not_refuse() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let node = connect_node(
        hub.addr,
        &enroll_token(hub.addr, &cookie).await?,
        &host_id,
        json!({ "cpuPct": 100, "memPct": 40 }),
    )
    .await?;
    // `None`: the Node never answers host.resources; the bounded refresh
    // expires and placement must not refuse on the fossil sample.
    let _node = serve_node(node, None);

    tokio::time::sleep(Duration::from_millis(350)).await;

    let body = json!({
        "placement": { "kind": "any" },
        "driver": "claude-print",
        "delegation": "none"
    });
    let (status, response) = create(hub.addr, &cookie, body).await?;
    assert_eq!(
        status,
        200,
        "a stale, unconfirmable sample never 422s: {response}"
    );
    Ok(())
}

#[tokio::test]
async fn fresh_saturation_refuses_auto_but_pins_warn_and_admit() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new().as_id().as_str().to_string();
    let node = connect_node(
        hub.addr,
        &enroll_token(hub.addr, &cookie).await?,
        &host_id,
        json!({ "cpuPct": 100, "memPct": 40 }),
    )
    .await?;
    let _node = serve_node(node, Some(json!({ "cpuPct": 100, "memPct": 40 })));

    // Fresh sample (hello just happened): auto-placement still refuses.
    let body = json!({
        "placement": { "kind": "any" },
        "driver": "claude-print",
        "delegation": "none"
    });
    let (status, response) = create(hub.addr, &cookie, body).await?;
    assert_eq!(status, 422, "{response}");
    assert_eq!(response["code"], json!("PLACEMENT_UNSATISFIABLE"));
    let reasons = response["reasons"].as_array().cloned().unwrap_or_default();
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap_or("").contains("CPU at 100%")),
        "{reasons:?}"
    );

    // The same fresh 100% on an explicitly requested host is a warning, not a
    // refusal.
    let body = json!({
        "hostId": host_id,
        "driver": "claude-print",
        "delegation": "none"
    });
    let (status, response) = create(hub.addr, &cookie, body).await?;
    assert_eq!(status, 200, "pin admits despite saturation: {response}");
    assert_eq!(response["hostId"], json!(host_id));
    let warnings = response["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("CPU at 100%")),
        "{warnings:?}"
    );
    let instance_id = response["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?;

    // The warning is journaled against the created instance.
    let (status, _, journal_body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}/journal"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{journal_body}");
    let journal = serde_json::from_str::<Value>(journal_body.trim())?;
    let events = journal["events"].as_array().cloned().unwrap_or_default();
    assert!(
        events.iter().any(|record| {
            let event = &record["event"];
            event["payload"]["nativeName"] == json!("placement_resource_warning")
                && event["payload"]["severity"] == json!("warning")
                && event["payload"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("CPU at 100%"))
        }),
        "placement warning must be journaled: {journal}"
    );
    Ok(())
}
