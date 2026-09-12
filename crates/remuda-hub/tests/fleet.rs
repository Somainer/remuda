//! Two fake Nodes: placement, fleet fan-out, offline skip, unsatisfiable.

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

async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
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
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "fleet-test" }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    cookie_from(&head).context("set-cookie")
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
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

async fn connect_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &str,
    label: &str,
    labels: Value,
    herdr: Option<Value>,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    Option<String>,
)> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": label,
                "host": {
                    "labels": labels,
                    "maxInstances": 4,
                    "cli": [{ "kind": "claude", "version": "2.1.268", "path": "/usr/bin/claude", "auth": "unknown" }],
                    "herdr": herdr,
                    "resources": { "cpuPct": 1, "memPct": 2 }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["hostId"] == host_id, "{hello}");
    let token = hello["result"]["nodeToken"].as_str().map(str::to_string);
    Ok((node, token))
}

#[tokio::test]
async fn two_nodes_placement_fleet_and_unsatisfiable() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    let sg = HostId::new();
    let cn = HostId::new();
    let sg_id = sg.as_id().as_str().to_string();
    let cn_id = cn.as_id().as_str().to_string();

    let (_sg_ws, _) = connect_node(
        hub.addr,
        &bootstrap,
        &sg_id,
        "sg-node",
        json!({ "region": "sg", "egress": "gateway" }),
        Some(json!({ "version": "0.9.0", "socket": "/tmp/herdr.sock" })),
    )
    .await?;
    let (mut cn_ws, cn_token) = connect_node(
        hub.addr,
        &bootstrap,
        &cn_id,
        "cn-node",
        json!({ "region": "cn" }),
        None,
    )
    .await?;
    let cn_token = cn_token.context("cn nodeToken")?;

    let (status, _, hosts) = http(hub.addr, "GET", "/v1/hosts", &auth, None).await?;
    assert_eq!(status, 200, "{hosts}");
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    assert_eq!(hosts["items"].as_array().map(|a| a.len()), Some(2));

    let patch =
        json!({ "labels": ["region=sg", "egress=gateway", "tier=canary"], "maxInstances": 3 })
            .to_string();
    let (status, _, patched) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/hosts/{sg_id}"),
        &auth,
        Some(&patch),
    )
    .await?;
    assert_eq!(status, 200, "{patched}");
    let patched: Value = serde_json::from_str(patched.trim())?;
    assert_eq!(patched["maxInstances"], json!(3));
    assert_eq!(patched["name"], json!("sg-node"));

    let body = json!({
        "placement": { "kind": "labels", "labels": ["region=sg"] },
        "driver": "claude-print",
        "prompt": "sg only"
    })
    .to_string();
    let (status, _, created) = http(hub.addr, "POST", "/v1/instances", &auth, Some(&body)).await?;
    assert_eq!(status, 200, "{created}");
    let created: Value = serde_json::from_str(created.trim())?;
    assert_eq!(created["hostId"], json!(sg_id));

    let body = json!({
        "hostId": cn_id,
        "driver": "claude-pty"
    })
    .to_string();
    let (status, _, err) = http(hub.addr, "POST", "/v1/instances", &auth, Some(&body)).await?;
    assert_eq!(status, 422, "{err}");
    let err: Value = serde_json::from_str(err.trim())?;
    assert_eq!(err["code"], json!("PLACEMENT_UNSATISFIABLE"));
    let reasons = err["reasons"].as_array().cloned().unwrap_or_default();
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap_or("").contains("herdr")),
        "{reasons:?}"
    );

    let body = json!({
        "placement": { "kind": "any" },
        "driver": "claude-print",
        "delegation": "gateway"
    })
    .to_string();
    let (status, _, created) = http(hub.addr, "POST", "/v1/instances", &auth, Some(&body)).await?;
    assert_eq!(status, 200, "{created}");
    let created: Value = serde_json::from_str(created.trim())?;
    assert_eq!(created["hostId"], json!(sg_id));

    cn_ws.close(None).await.ok();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let body = json!({
        "spec": { "driver": "claude-print", "kind": "claude" },
        "hosts": [sg_id, cn_id]
    })
    .to_string();
    let (status, _, err) =
        http(hub.addr, "POST", "/v1/fleet/instances", &auth, Some(&body)).await?;
    assert_eq!(status, 422, "{err}");

    let (_cn_ws, _) = connect_node(
        hub.addr,
        &cn_token,
        &cn_id,
        "cn-node",
        json!({ "region": "cn" }),
        None,
    )
    .await?;

    let body = json!({
        "spec": { "driver": "claude-print", "kind": "claude", "prompt": "fleet" },
        "hosts": [sg_id, cn_id]
    })
    .to_string();
    let (status, _, fleet) =
        http(hub.addr, "POST", "/v1/fleet/instances", &auth, Some(&body)).await?;
    assert_eq!(status, 200, "{fleet}");
    let fleet: Value = serde_json::from_str(fleet.trim())?;
    let fleet_id = fleet["fleetId"].as_str().context("fleetId")?;
    assert_eq!(fleet["instanceIds"].as_array().map(|a| a.len()), Some(2));

    let (status, _, got) = http(
        hub.addr,
        "GET",
        &format!("/v1/fleet/{fleet_id}"),
        &auth,
        None,
    )
    .await?;
    assert_eq!(status, 200, "{got}");
    let got: Value = serde_json::from_str(got.trim())?;
    assert_eq!(got["instances"].as_array().map(|a| a.len()), Some(2));

    let cmd = json!({ "operation": "instance.send", "payload": { "text": "hi" } }).to_string();
    let (status, _, cmds) = http(
        hub.addr,
        "POST",
        &format!("/v1/fleet/{fleet_id}/commands"),
        &auth,
        Some(&cmd),
    )
    .await?;
    assert_eq!(status, 200, "{cmds}");
    let cmds: Value = serde_json::from_str(cmds.trim())?;
    assert_eq!(cmds["commands"].as_array().map(|a| a.len()), Some(2));
    let states: Vec<&str> = cmds["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["state"].as_str())
        .collect();
    assert!(states.iter().all(|s| *s == "queued" || *s == "accepted"));
    Ok(())
}
