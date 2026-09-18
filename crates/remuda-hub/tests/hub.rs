//! Drive Hub HTTP + `/v1/node` + `/v1/follow` with a fake Node client.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

#[tokio::test]
async fn offline_host_exits_in_background_and_is_only_listed_in_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.host_lost_grace_ms = 20;
    let hub = spawn(config).await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &hub.bootstrap_token).await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    let instance_id = InstanceId::new();
    node.send(Message::Text(json!({"jsonrpc":"2.0", "id":"1", "method":"node.hello",
        "params":{"hostId":HostId::new().as_id().as_str(), "nodeVersion":"test", "label":"reclaim-test"}
    }).to_string().into())).await?;
    assert!(recv_json(&mut node).await?.get("result").is_some());
    let event: Value = serde_json::from_str(include_str!("fixtures/journal-event.json"))?;
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"2", "method":"journal.append",
            "params":{"instanceId":instance_id.as_id().as_str(), "event":event}
        })
        .to_string()
        .into(),
    ))
    .await?;
    assert!(recv_json(&mut node).await?.get("result").is_some());
    let (_, _, body) = http(
        hub.addr,
        "GET",
        "/v1/instances",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(
        serde_json::from_str::<Value>(&body)?["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    node.close(None).await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let (_, _, body) = http(
                hub.addr,
                "GET",
                "/v1/instances",
                &[("Cookie", &cookie)],
                None,
            )
            .await?;
            if serde_json::from_str::<Value>(&body)?["items"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let (_, _, body) = http(
        hub.addr,
        "GET",
        "/v1/instances?includeHistory=true",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    let history: Value = serde_json::from_str(&body)?;
    assert_eq!(history["items"][0]["lifecycle"], "exited");
    assert_eq!(history["items"][0]["lastError"], "host-lost");
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{}", instance_id.as_id()),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    hub.shutdown().await;
    Ok(())
}

async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
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
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            let token = value.split(';').next()?.trim();
            return Some(token.to_string());
        }
    }
    None
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<(String, String)> {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "test-phone"
    })
    .to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    let cookie = cookie_from(&head).context("set-cookie")?;
    let json: Value = serde_json::from_str(rest.trim())?;
    let token = json
        .get("token")
        .and_then(Value::as_str)
        .context("device token")?
        .to_string();
    Ok((cookie, token))
}

/// Mint a single-use Node enroll token with a paired device's cookie (D-018).
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
    let value: Value = serde_json::from_str(rest.trim())?;
    value["token"]
        .as_str()
        .map(str::to_string)
        .context("enroll token")
}

/// Poll the host index until the Hub's live-link projection reports the
/// expected `online` value. Closing a WebSocket is observed on the server
/// asynchronously, so a fixed sleep before the assertion races that
/// observation under `--test-threads` load.
async fn wait_host_online(
    addr: std::net::SocketAddr,
    cookie: &str,
    host_id: &str,
    expected: bool,
) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let (status, _, body) =
                http(addr, "GET", "/v1/hosts", &[("Cookie", cookie)], None).await?;
            anyhow::ensure!(status == 200, "list hosts {status} {body}");
            let hosts: Value = serde_json::from_str(body.trim())?;
            let current = hosts["items"]
                .as_array()
                .and_then(|items| items.iter().find(|host| host["hostId"] == json!(host_id)))
                .and_then(|host| host["online"].as_bool());
            if current == Some(expected) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("host {host_id} did not reach online={expected} within {TIMEOUT:?}"))?
}

/// Pair a device and mint an enroll token in one step.
async fn device_and_enroll(
    addr: std::net::SocketAddr,
    bootstrap: &str,
) -> Result<(String, String, String)> {
    let (cookie, token) = login(addr, bootstrap).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    Ok((cookie, token, enroll))
}

#[tokio::test]
async fn healthz_ok() -> Result<()> {
    let (hub, _, _dir) = boot().await?;
    let (status, _, body) = http(hub.addr, "GET", "/healthz", &[], None).await?;
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true") || body.contains("\"ok\": true"));
    Ok(())
}

#[tokio::test]
async fn instance_configure_is_journaled_and_persisted() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0", "label": "configure-node" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let method = frame["method"].as_str().unwrap_or_default();
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            let command_id = params
                .get("commandId")
                .cloned()
                .unwrap_or_else(|| json!("cmd_test"));
            let _ = node
                .send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "command": {
                                "commandId": command_id,
                                "state": "accepted",
                                "operation": method
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await;
        }
    });

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "model": "haiku",
        "prompt": "configure-me",
        "effort": { "index": 1, "name": "think", "kind": "claude" }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(body.trim())?;
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();
    // D-028 §9.1: a pre-D-028 client still posts `{index, name: "think"}`.
    // The Hub normalizes the tier by NAME and answers with the level, so the
    // old client keeps working and every reader sees one vocabulary.
    assert_eq!(created["instance"]["effortName"], json!("high"));
    assert_eq!(created["instance"]["effortUltracode"], json!(false));
    assert_eq!(created["instance"]["effortIndex"], json!(1));

    let configure = json!({
        "operation": "instance.configure",
        "payload": {
            "model": "opus",
            "effort": { "index": 3, "name": "ultracode", "kind": "claude" }
        }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        &[("Cookie", &cookie)],
        Some(&configure),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["command"]["operation"], json!("instance.configure"));
    assert_eq!(
        body["command"]["payload"]["effort"]["name"],
        json!("ultracode")
    );
    assert_eq!(body["command"]["payload"]["effort"]["index"], json!(3));
    assert!(
        matches!(
            body["command"]["state"].as_str(),
            Some("queued" | "accepted" | "settled")
        ),
        "three-state command, got {}",
        body["command"]["state"]
    );

    let (status, _, inst) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{inst}");
    let inst: Value = serde_json::from_str(inst.trim())?;
    assert_eq!(inst["model"], json!("opus"));
    // `ultracode` is xhigh plus the dynamic-workflow flag, never a level.
    assert_eq!(inst["effortName"], json!("xhigh"));
    assert_eq!(inst["effortUltracode"], json!(true));
    // A D-028 client posting the new shape reaches the same stored state.
    let configure = json!({
        "operation": "instance.configure",
        "payload": { "model": "opus", "effort": { "name": "xhigh", "ultracode": true } }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        &[("Cookie", &cookie)],
        Some(&configure),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let (status, _, inst) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{inst}");
    let inst: Value = serde_json::from_str(inst.trim())?;
    assert_eq!(inst["effortName"], json!("xhigh"));
    assert_eq!(inst["effortUltracode"], json!(true));
    // §1.0 rule 4: provenance is reported for a Remuda-launched instance.
    assert_eq!(inst["launchedBy"], json!("remuda"));
    Ok(())
}

#[tokio::test]
async fn a_send_the_node_rejects_resolves_failed_and_is_listed_by_the_new_route() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // A short ack deadline so the deadline path (a node that never replies)
    // also resolves inside the test window.
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.command_settle_timeout_ms = 300;
    let hub = spawn(config).await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &hub.bootstrap_token).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0", "label": "reject-send" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    // Accept the create so the instance is reachable, but reject instance.send
    // exactly the way the ssh-stdio Node did before the fold — with an
    // invalid-request error, so the Hub must fail the row rather than leave it
    // queued (docs/design/evidence/instance-send-1.md).
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let method = frame["method"].as_str().unwrap_or_default().to_owned();
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            let command_id = params
                .get("commandId")
                .cloned()
                .unwrap_or_else(|| json!("cmd_test"));
            let reply = if method == "instance.send" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "invalid request: instance.send requires input.text or input.blocks" }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "command": { "commandId": command_id, "state": "accepted", "operation": method }
                    }
                })
            };
            let _ = node.send(Message::Text(reply.to_string().into())).await;
        }
    });

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "prompt": "hi"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(body.trim())?;
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();

    let send = json!({
        "operation": "instance.send",
        "payload": { "input": { "type": "prompt", "blocks": [{ "type": "text", "text": "hello" }] } }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        &[("Cookie", &cookie)],
        Some(&send),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    let command_id = body["command"]["commandId"]
        .as_str()
        .context("commandId")?
        .to_string();
    assert_eq!(
        body["command"]["state"],
        json!("failed"),
        "a rejected send must resolve failed, not queued: {body}"
    );
    let reason = body["command"]["reason"]
        .as_str()
        .context("failure reason present")?;
    assert!(
        reason.contains("input.text") || reason.contains("input.blocks"),
        "the reason carries the node's message: {reason}"
    );

    // The new GET route lists the command, newest first, with its resolved state.
    let (status, _, listed) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}/commands"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    let commands = listed["commands"].as_array().context("commands array")?;
    let row = commands
        .iter()
        .find(|row| row["commandId"].as_str() == Some(command_id.as_str()))
        .context("send command listed")?;
    assert_eq!(row["operation"], json!("instance.send"));
    assert_eq!(row["state"], json!("failed"));
    assert_eq!(row["resolution"], json!("failed"));
    assert!(row.get("reason").and_then(Value::as_str).is_some());
    assert!(row.get("createdAt").is_some() && row.get("updatedAt").is_some());
    Ok(())
}

#[tokio::test]
async fn auth_reject_http_and_origin() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (status, _, _) = http(hub.addr, "GET", "/v1/hosts", &[], None).await?;
    assert_eq!(status, 401);

    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "x" }).to_string();
    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/login",
        &[("Origin", "http://evil.example")],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 403);

    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/login",
        &[],
        Some(&json!({"bootstrapToken":"nope","deviceName":"x"}).to_string()),
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

#[tokio::test]
async fn login_and_pair_attempts_are_limited_by_peer_not_forwarded_headers() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    for (path, body) in [
        ("/v1/login", json!({"bootstrapToken":"wrong"})),
        ("/v1/devices/pair", json!({"code":"ZZZZZZZZ"})),
    ] {
        for i in 0..10 {
            let forwarded = format!("192.0.2.{i}");
            let (status, _, _) = http(
                hub.addr,
                "POST",
                path,
                &[("X-Forwarded-For", &forwarded)],
                Some(&body.to_string()),
            )
            .await?;
            assert_eq!(status, 401, "{path} attempt {i}");
        }
        let (status, headers, body) = http(
            hub.addr,
            "POST",
            path,
            &[("Forwarded", "for=198.51.100.1")],
            Some(&body.to_string()),
        )
        .await?;
        assert_eq!(status, 429, "{path}: {body}");
        assert!(headers.to_ascii_lowercase().contains("retry-after: 10"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-frame-options: deny")
        );
    }
    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/login",
        &[],
        Some(&json!({"bootstrapToken":bootstrap}).to_string()),
    )
    .await?;
    assert_eq!(
        status, 429,
        "rate limit must precede credential verification"
    );
    let (status, _, _) = http(hub.addr, "GET", "/healthz", &[], None).await?;
    assert_eq!(status, 200);
    Ok(())
}

#[tokio::test]
async fn node_ws_rejects_missing_token() -> Result<()> {
    let (hub, _, _dir) = boot().await?;
    let url = format!("ws://{}/v1/node", hub.addr);
    let err = tokio_tungstenite::connect_async(url).await.err();
    assert!(err.is_some(), "unauthenticated node socket must fail");
    Ok(())
}

#[tokio::test]
async fn fake_node_hello_heartbeat_append_then_http_and_follow() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect timeout")??;

    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0-test",
                "label": "fake-node",
                "host": {
                    "hostname": "fake-node.local",
                    "labels": { "region": "sg", "role": "canary" },
                    "maxInstances": 4,
                    "cli": [{
                        "kind": "claude",
                        "version": "2.1.268",
                        "absolutePath": "/usr/bin/claude",
                        "authState": "unknown"
                    }],
                    "herdr": { "version": "0.9.0", "socket": "/tmp/herdr.sock" },
                    "resources": { "cpuPct": 8, "memPct": 31 }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    assert_eq!(hello["id"], "1");
    assert!(hello["result"]["nodeToken"].as_str().is_some());
    assert_eq!(hello["result"]["protocol"]["major"], json!(1));

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "node.heartbeat",
            "params": {
                "cli": [{
                    "kind": "claude",
                    "version": "2.1.268",
                    "path": "/usr/bin/claude",
                    "auth": "logged_in"
                }],
                "herdr": { "version": "0.9.1", "socket": "/tmp/herdr.sock" },
                "resources": { "cpuPct": 12, "memPct": 40 },
                "maxInstances": 8
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let beat = recv_json(&mut node).await?;
    assert_eq!(beat["id"], "2");
    assert!(beat.get("result").is_some());

    let event: Value = serde_json::from_str(include_str!("fixtures/journal-event.json"))?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": event
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let appended = recv_json(&mut node).await?;
    assert_eq!(appended["result"]["seq"], json!("1"));

    let (status, _, hosts) =
        http(hub.addr, "GET", "/v1/hosts", &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200);
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    assert_eq!(hosts["items"][0]["online"], json!(true));
    assert_eq!(hosts["items"][0]["label"], json!("fake-node"));
    assert_eq!(hosts["items"][0]["transport"], json!("outbound-wss"));
    assert_eq!(hosts["items"][0]["cli"][0]["kind"], json!("claude"));
    assert_eq!(hosts["items"][0]["cli"][0]["auth"], json!("logged_in"));
    assert_eq!(hosts["items"][0]["herdr"]["version"], json!("0.9.1"));
    assert_eq!(hosts["items"][0]["resources"]["cpuPct"], json!(12));
    assert_eq!(hosts["items"][0]["maxInstances"], json!(8));
    let tags = hosts["items"][0]["labels"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(tags.iter().any(|t| t == "region=sg"));

    let journal_path = format!("/v1/instances/{}/journal", instance_id.as_id().as_str());
    let (status, _, journal) =
        http(hub.addr, "GET", &journal_path, &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200, "{journal}");
    let journal: Value = serde_json::from_str(journal.trim())?;
    assert_eq!(journal["durableSeq"], json!("1"));
    assert!(
        journal["events"][0]["event"]["payload"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("fake-node")
    );

    let mut follow_req = format!(
        "ws://{}/v1/follow?instanceId={}",
        hub.addr,
        instance_id.as_id().as_str()
    )
    .into_client_request()?;
    follow_req
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req))
            .await
            .context("follow connect")??;
    let snapshot = recv_json(&mut follow).await?;
    assert_eq!(snapshot["type"], json!("snapshot"));
    assert_eq!(snapshot["asOfSeq"], json!("1"));
    assert_eq!(snapshot["events"].as_array().map(|a| a.len()), Some(1));

    // Second append should arrive as a live follow event.
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "4",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": { "kind": "message", "payload": { "text": "second" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    let live = recv_json(&mut follow).await?;
    assert_eq!(live["type"], json!("event"));
    assert_eq!(live["seq"], json!("2"));
    Ok(())
}

#[tokio::test]
async fn command_stays_queued_when_node_offline_and_is_not_resent() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    // Enroll a host then drop the socket so it is offline.
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .context("nodeToken")?
        .to_string();
    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "prompt": "hi"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    let instance_id = body["instance"]["instanceId"].as_str().unwrap().to_string();
    node.close(None).await.ok();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;

    let (status, _, hosts) =
        http(hub.addr, "GET", "/v1/hosts", &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200, "{hosts}");
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    assert_eq!(hosts["items"][0]["online"], json!(false), "{hosts}");

    let send = json!({
        "operation": "instance.send",
        "payload": { "text": "queued-offline" }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        &[("Cookie", &cookie)],
        Some(&send),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["command"]["state"], json!("queued"));
    assert_eq!(body["command"]["forwarded"], json!(false));

    let create_offline = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "prompt": "offline-create"
    })
    .to_string();
    let (status, _, rejected) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create_offline),
    )
    .await?;
    assert_eq!(status, 409, "{rejected}");
    let rejected: Value = serde_json::from_str(rejected.trim())?;
    assert_eq!(rejected["code"], json!("HOST_OFFLINE"));
    let command_id = body["command"]["commandId"].as_str().unwrap().to_string();

    // Reconnect: Hub must not auto-resend the queued command.
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h2",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str() }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    let raced = tokio::time::timeout(Duration::from_millis(250), recv_json(&mut node)).await;
    assert!(raced.is_err(), "reconnect must not replay commands");

    // Same commandId + payload is idempotent (reconnect must not create a second native send).
    let replay = json!({
        "commandId": command_id,
        "operation": "instance.send",
        "payload": body["command"]["payload"]
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&replay),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["replayed"], json!(true));
    Ok(())
}

#[tokio::test]
async fn restart_marks_hosts_offline_and_rejects_create() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let config = HubConfig::for_test(data_dir.clone());
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let (node, _) = {
        let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
        let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
            .await
            .context("connect")??;
        node.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "h",
                "method": "node.hello",
                "params": { "hostId": host_id.as_id().as_str(), "label": "stale-check" }
            })
            .to_string()
            .into(),
        ))
        .await?;
        let hello = recv_json(&mut node).await?;
        anyhow::ensure!(hello.get("result").is_some(), "{hello}");
        (node, hello)
    };
    drop(node);
    hub.shutdown().await;

    let mut config = HubConfig::for_test(data_dir);
    config.bootstrap_token = bootstrap;
    let hub = spawn(config).await?;
    let (status, _, hosts) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{hosts}");
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    assert_eq!(hosts["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(hosts["items"][0]["online"], json!(false), "{hosts}");
    assert_eq!(hosts["items"][0]["state"], json!("offline"));

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "prompt": "no-link"
    })
    .to_string();
    let (status, _, rejected) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 409, "{rejected}");
    let rejected: Value = serde_json::from_str(rejected.trim())?;
    assert_eq!(rejected["code"], json!("HOST_OFFLINE"));
    assert_eq!(rejected["hostId"].as_str(), Some(host_id.as_id().as_str()));
    Ok(())
}

#[tokio::test]
async fn node_cannot_append_to_another_hosts_journal() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node_a, _) = tokio_tungstenite::connect_async(req).await?;
    let host_a = HostId::new();
    let instance = InstanceId::new();
    node_a
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "a",
                "method": "runtime.hello",
                "params": { "hostId": host_a.as_id().as_str() }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let _ = recv_json(&mut node_a).await?;
    node_a
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "a2",
                "method": "journal.append",
                "params": {
                    "instanceId": instance.as_id().as_str(),
                    "event": { "kind": "message", "payload": { "text": "owner" } }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let appended = recv_json(&mut node_a).await?;
    assert_eq!(appended["result"]["seq"], json!("1"));

    let enroll_b = enroll_token(hub.addr, &cookie).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {enroll_b}").parse().unwrap(),
    );
    let (mut node_b, _) = tokio_tungstenite::connect_async(req).await?;
    let host_b = HostId::new();
    node_b
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "b",
                "method": "runtime.hello",
                "params": { "hostId": host_b.as_id().as_str() }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let _ = recv_json(&mut node_b).await?;
    node_b
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "b2",
                "method": "journal.append",
                "params": {
                    "instanceId": instance.as_id().as_str(),
                    "event": { "kind": "message", "payload": { "text": "stolen" } }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let denied = recv_json(&mut node_b).await?;
    assert_eq!(denied["error"]["code"], json!(-32001));
    Ok(())
}

#[tokio::test]
async fn follow_uses_cookie_or_header_and_never_query_token() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, token, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0", "label": "token-node" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ))
                .await;
        }
    });

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "prompt": "token-follow"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Authorization", &format!("Bearer {token}"))],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    let instance_id = body["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();

    let (status, _, got) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Authorization", &format!("Bearer {token}"))],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{got}");
    let got: Value = serde_json::from_str(got.trim())?;
    assert_eq!(got["instanceId"], json!(instance_id));

    let query_req = format!(
        "ws://{}/v1/follow?instanceId={}&token={}",
        hub.addr, instance_id, token
    );
    let rejected = tokio_tungstenite::connect_async(query_req)
        .await
        .err()
        .context("query token must fail")?;
    assert!(
        matches!(rejected, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 401)
    );
    for (header, value) in [
        ("Cookie", cookie),
        ("Authorization", format!("Bearer {token}")),
    ] {
        let mut follow_req = format!("ws://{}/v1/follow?instanceId={instance_id}", hub.addr)
            .into_client_request()?;
        follow_req.headers_mut().insert(header, value.parse()?);
        let (mut follow, response) =
            tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req)).await??;
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        let snapshot = recv_json(&mut follow).await?;
        assert_eq!(snapshot["type"], json!("snapshot"));
        assert_eq!(snapshot["instanceId"], json!(instance_id));
    }
    Ok(())
}

#[tokio::test]
async fn revoking_current_device_expires_httponly_cookie() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, token) = login(hub.addr, &bootstrap).await?;
    let (_, _, body) = http(hub.addr, "GET", "/v1/devices", &[("Cookie", &cookie)], None).await?;
    let devices: Value = serde_json::from_str(body.trim())?;
    let device_id = devices["items"][0]["id"].as_str().context("device id")?;
    let (status, headers, _) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/devices/{device_id}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(headers.contains("HttpOnly; SameSite=Strict; Max-Age=0"));
    assert_eq!(cookie_from(&headers).as_deref(), Some("remuda_device="));
    let (status, _, _) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Authorization", &format!("Bearer {token}"))],
        None,
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

#[tokio::test]
async fn create_instance_persists_delegation_and_provider_profile() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "label": "local-development",
                "host": {
                    "hostname": "local-development",
                    "labels": { "egress": "gateway" },
                    "maxInstances": 4
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ))
                .await;
        }
    });

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "providerProfileId": "gateway",
        "delegation": "none",
        "maxBudgetUsd": "0.3",
        "permissionMode": "bypassPermissions",
        "prompt": "persist-delegation"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["instance"]["delegation"], json!("none"));
    assert_eq!(body["instance"]["providerProfileId"], json!("none"));
    assert_eq!(body["instance"]["providerSource"], json!("request"));
    let instance_id = body["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();

    let (status, _, got) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{got}");
    let got: Value = serde_json::from_str(got.trim())?;
    assert_eq!(got["delegation"], json!("none"));
    assert_eq!(got["providerProfileId"], json!("none"));

    let (status, _, listed) = http(
        hub.addr,
        "GET",
        "/v1/instances",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    let item = listed["items"]
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .context("listed instance")?;
    assert_eq!(item["delegation"], json!("none"));
    assert_eq!(item["providerProfileId"], json!("none"));
    Ok(())
}

/// Per-host launch defaults: PATCH round-trip, create-time merge, and the
/// session-replaces-default rule.
#[tokio::test]
async fn host_launch_defaults_round_trip_and_a_session_replaces_them() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "label": "local-development",
                "host": { "hostname": "local-development", "maxInstances": 4 }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let sink = seen.clone();
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            sink.lock().expect("lock").push(frame.clone());
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ))
                .await;
        }
    });

    let host_path = format!("/v1/hosts/{}", host_id.as_id().as_str());

    // PATCH stores launch defaults and the view reports them back.
    let (status, _, patched) = http(
        hub.addr,
        "PATCH",
        &host_path,
        &[("Cookie", &cookie)],
        Some(
            &json!({
                "defaultLaunchArgs": ["--effort", "high"],
                "claudeBinaryPath": "/opt/claude/bin/claude",
                "defaultTui": "default"
            })
            .to_string(),
        ),
    )
    .await?;
    assert_eq!(status, 200, "{patched}");
    let patched: Value = serde_json::from_str(patched.trim())?;
    assert_eq!(patched["defaultLaunchArgs"], json!(["--effort", "high"]));
    assert_eq!(patched["claudeBinaryPath"], json!("/opt/claude/bin/claude"));

    assert_eq!(patched["defaultTui"], json!("default"));
    let (status, _, loaded) =
        http(hub.addr, "GET", &host_path, &[("Cookie", &cookie)], None).await?;
    assert_eq!(status, 200, "{loaded}");
    assert_eq!(
        serde_json::from_str::<Value>(loaded.trim())?["defaultTui"],
        json!("default")
    );

    // A flag the allowlist refuses is a 400 on the PATCH, not a surprise at
    // the next launch.
    let (status, _, rejected) = http(
        hub.addr,
        "PATCH",
        &host_path,
        &[("Cookie", &cookie)],
        Some(&json!({ "defaultLaunchArgs": ["--dangerously-skip-permissions"] }).to_string()),
    )
    .await?;
    assert_eq!(status, 400, "{rejected}");

    // A create that omits launch preferences inherits the host defaults.
    let create = |extra: Value| {
        let mut body = json!({
            "hostId": host_id.as_id().as_str(),
            "kind": "claude",
            "driver": "claude-print",
            "permissionMode": "manual",
            "prompt": "defaults"
        });
        if let (Some(obj), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                obj.insert(key.clone(), value.clone());
            }
        }
        body.to_string()
    };
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create(json!({}))),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    assert_eq!(
        serde_json::from_str::<Value>(body.trim())?["instance"]["tui"],
        json!("default")
    );

    // A session value REPLACES the host default rather than concatenating:
    // two arg lists merged would repeat a flag, which the allowlist refuses.
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create(json!({
            "args": ["--effort", "low"],
            "binaryPath": "/opt/claude-2.2/bin/claude",
            "tui": "fullscreen"
        }))),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    let frames = seen.lock().expect("lock").clone();
    let specs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["method"] == json!("instance.create"))
        .map(|f| &f["params"]["spec"])
        .collect();
    assert_eq!(specs.len(), 2, "two creates were forwarded: {frames:?}");
    assert_eq!(specs[0]["args"], json!(["--effort", "high"]));
    assert_eq!(specs[0]["binaryPath"], json!("/opt/claude/bin/claude"));
    assert_eq!(specs[0]["tui"], json!("default"));
    assert_eq!(specs[1]["tui"], json!("fullscreen"));
    assert_eq!(specs[1]["args"], json!(["--effort", "low"]));
    assert_eq!(
        specs[1]["binaryPath"],
        json!("/opt/claude-2.2/bin/claude"),
        "the session value must win outright"
    );

    // A bad session flag is a 400 from the same table the Node uses.
    let (status, _, refused) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create(json!({ "args": ["--bare"] }))),
    )
    .await?;
    assert_eq!(status, 400, "{refused}");

    // Clearing a default is distinct from never setting one.
    let (status, _, cleared) = http(
        hub.addr,
        "PATCH",
        &host_path,
        &[("Cookie", &cookie)],
        Some(
            &json!({ "defaultLaunchArgs": null, "claudeBinaryPath": null, "defaultTui": null })
                .to_string(),
        ),
    )
    .await?;
    assert_eq!(status, 200, "{cleared}");
    let cleared: Value = serde_json::from_str(cleared.trim())?;
    assert!(cleared["defaultLaunchArgs"].is_null(), "{cleared}");
    assert!(cleared["claudeBinaryPath"].is_null(), "{cleared}");
    assert!(cleared["defaultTui"].is_null(), "{cleared}");
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create(json!({}))),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(body.trim())?["instance"]["tui"],
        json!("fullscreen")
    );
    let frames = seen.lock().expect("lock");
    let last = frames
        .iter()
        .rev()
        .find(|frame| frame["method"] == json!("instance.create"))
        .expect("third create");
    assert_eq!(last["params"]["spec"]["tui"], json!("fullscreen"));
    Ok(())
}

/// An instance never inherits operator authority, so it picks neither its own
/// flags nor its own executable.
#[tokio::test]
async fn agent_scoped_create_cannot_set_args_or_binary_path() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "label": "local-development",
                "host": { "hostname": "local-development", "maxInstances": 8 }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ))
                .await;
        }
    });

    let parent_body = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "permissionMode": "manual",
        // §2.5: delegating a child needs the dispatch grant; this test's
        // subject is the args/binaryPath refusal, not delegation itself.
        "grants": ["dispatch"],
        "prompt": "parent"
    })
    .to_string();
    let (status, _, created) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&parent_body),
    )
    .await?;
    assert_eq!(status, 200, "{created}");
    let created: Value = serde_json::from_str(created.trim())?;
    let parent = created["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();

    for extra in [
        json!({ "args": ["--effort", "high"] }),
        json!({ "binaryPath": "/opt/claude/bin/claude" }),
    ] {
        let mut body = json!({
            "hostId": host_id.as_id().as_str(),
            "kind": "claude",
            "driver": "claude-print",
            "permissionMode": "manual",
            "prompt": "child"
        });
        if let (Some(obj), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                obj.insert(key.clone(), value.clone());
            }
        }
        let (status, _, refused) = http(
            hub.addr,
            "POST",
            "/v1/instances",
            &[("Cookie", &cookie), ("x-remuda-instance-id", &parent)],
            Some(&body.to_string()),
        )
        .await?;
        assert_eq!(status, 403, "{extra} -> {refused}");
    }

    // Without either field the same agent-scoped create still works, so the
    // gate is the fields and not the caller being blocked outright.
    let plain = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "permissionMode": "manual",
        "prompt": "child-plain"
    })
    .to_string();
    let (status, _, ok) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie), ("x-remuda-instance-id", &parent)],
        Some(&plain),
    )
    .await?;
    assert_eq!(status, 200, "{ok}");
    Ok(())
}

#[tokio::test]
async fn create_instance_fails_when_node_rejects_cwd() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "label": "local-development"
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32602,
                            "message": "invalid request: cwd must resolve inside the registered workspace or a worktree beside it: /private/tmp resolves outside /tmp/workspace"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await;
        }
    });

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "permissionMode": "manual",
        "delegation": "none",
        "cwd": "/tmp",
        "prompt": "Use Bash to write a file"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", &cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["code"], json!("BAD_REQUEST"));
    let message = body["error"].as_str().unwrap_or("");
    assert!(
        message.contains("cwd must resolve inside the registered workspace"),
        "{message}"
    );

    let (status, _, listed) = http(
        hub.addr,
        "GET",
        "/v1/instances",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    let item = listed["items"]
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .context("listed instance")?;
    assert_eq!(item["lifecycle"], json!("failed"), "{item}");
    assert_eq!(item["durableSeq"], json!("0"), "{item}");
    let last_error = item["lastError"].as_str().unwrap_or("");
    assert!(
        last_error.contains("cwd must resolve inside the registered workspace"),
        "{last_error}"
    );
    Ok(())
}

#[tokio::test]
async fn second_hello_on_socket_is_rejected() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (_cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    let host_id = HostId::new();
    let hello = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "node.hello",
        "params": { "hostId": host_id.as_id().as_str(), "label": "once" }
    });
    node.send(Message::Text(hello.to_string().into())).await?;
    let first = recv_json(&mut node).await?;
    assert!(first["result"]["nodeToken"].as_str().is_some(), "{first}");
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str() }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let second = recv_json(&mut node).await?;
    assert!(second.get("error").is_some(), "{second}");
    assert!(
        second["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("hello already")),
        "{second}"
    );
    Ok(())
}

#[tokio::test]
async fn node_auth_must_match_upgrade_bearer() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (_cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "a",
            "method": "node.auth",
            "params": { "token": "forged", "scheme": "bearer" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let denied = recv_json(&mut node).await?;
    assert_eq!(denied["error"]["code"], json!(-32000), "{denied}");
    Ok(())
}

#[tokio::test]
async fn journal_duplicate_seq_is_acked() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (_cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str() }
        })
        .to_string()
        .into(),
    ))
    .await?;
    recv_json(&mut node).await?;
    let event = json!({ "kind": "message", "payload": { "text": "once" } });
    for id in ["2", "3"] {
        node.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "journal.append",
                "params": {
                    "instanceId": instance_id.as_id().as_str(),
                    "seq": "1",
                    "event": event,
                    "events": [event.clone()]
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
        let ack = recv_json(&mut node).await?;
        assert_eq!(ack["result"]["seq"], json!("1"), "{ack}");
    }
    Ok(())
}

#[tokio::test]
async fn stale_socket_does_not_offline_live_host() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let mut req_a = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("a")?;
    req_a
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node_a, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req_a))
        .await
        .context("connect a")??;
    node_a
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "a1",
                "method": "node.hello",
                "params": { "hostId": host_id.as_id().as_str(), "label": "live" }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let hello_a = recv_json(&mut node_a).await?;
    let host_token = hello_a["result"]["nodeToken"]
        .as_str()
        .context("nodeToken")?
        .to_string();

    let mut req_b = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("b")?;
    req_b.headers_mut().insert(
        "Authorization",
        format!("Bearer {host_token}").parse().unwrap(),
    );
    let (mut node_b, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req_b))
        .await
        .context("connect b")??;
    node_b
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "b1",
                "method": "node.hello",
                "params": { "hostId": host_id.as_id().as_str() }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let hello_b = recv_json(&mut node_b).await?;
    assert!(hello_b.get("result").is_some(), "{hello_b}");

    drop(node_a);
    // The stale socket must be retired without offlining the host the live
    // socket (b) still serves. Poll across the close landing: a single read
    // could pass before the close is processed, while an unbounded wait-for
    // could never prove the close was absorbed.
    let fence_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let (status, _, hosts) = http(
            hub.addr,
            "GET",
            "/v1/hosts",
            &[("Cookie", cookie.as_str())],
            None,
        )
        .await?;
        assert_eq!(status, 200, "{hosts}");
        let hosts: Value = serde_json::from_str(hosts.trim())?;
        assert_eq!(
            hosts["items"][0]["online"],
            json!(true),
            "retiring the stale socket must not offline a live host: {hosts}"
        );
        if tokio::time::Instant::now() >= fence_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    node_b
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "b2",
                "method": "node.heartbeat",
                "params": {}
            })
            .to_string()
            .into(),
        ))
        .await?;
    let beat = recv_json(&mut node_b).await?;
    assert!(beat.get("result").is_some(), "{beat}");
    Ok(())
}

#[tokio::test]
async fn bootstrap_cannot_impersonate_existing_host_but_host_token_can_reconnect() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let hello = |version: &str| {
        json!({
            "jsonrpc": "2.0",
            "id": version,
            "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "label": "local-development",
                "nodeVersion": version,
                "cli": [
                    { "kind": "claude", "version": version, "path": "/usr/bin/claude", "auth": "unknown" },
                    { "kind": "codex", "version": "0.1.0", "path": "/usr/bin/codex", "auth": "unknown" },
                    { "kind": "grok", "version": "1.0.0", "path": "/usr/bin/grok", "auth": "unknown" }
                ]
            }
        })
        .to_string()
    };

    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("first node")?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("connect first")??;
    node.send(Message::Text(hello("1").into())).await?;
    let first = recv_json(&mut node).await?;
    let node_token = first["result"]["nodeToken"]
        .as_str()
        .context("nodeToken")?
        .to_string();

    // A1: a *fresh* enroll token cannot claim this live host's identity, and
    // the failed collision cannot steal or disconnect the victim's route.
    let attacker_enroll = enroll_token(hub.addr, &cookie).await?;
    let mut attacker_req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    attacker_req.headers_mut().insert(
        "Authorization",
        format!("Bearer {attacker_enroll}").parse()?,
    );
    let (mut attacker, _) = tokio_tungstenite::connect_async(attacker_req).await?;
    attacker
        .send(Message::Text(hello("attacker").into()))
        .await?;
    let rejected = recv_json(&mut attacker).await?;
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert!(rejected.get("result").is_none(), "{rejected}");
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"heartbeat", "method":"runtime.heartbeat", "params":{}})
            .to_string()
            .into(),
    ))
    .await?;
    assert!(recv_json(&mut node).await?.get("result").is_some());
    drop(attacker);
    drop(node);

    // D-018: the enroll token is spent; re-announce uses the stored node token.
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("second node")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("connect second")??;
    node.send(Message::Text(hello("2").into())).await?;
    let second = recv_json(&mut node).await?;
    assert!(second.get("result").is_some(), "{second}");
    assert_eq!(
        second["result"]["hostId"].as_str(),
        Some(host_id.as_id().as_str())
    );

    let (status, _, hosts) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{hosts}");
    let hosts: Value = serde_json::from_str(hosts.trim())?;
    let items = hosts["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{hosts}");
    assert_eq!(items[0]["hostId"], json!(host_id.as_id().as_str()));
    assert_eq!(items[0]["id"], json!(host_id.as_id().as_str()));
    assert_eq!(items[0]["online"], json!(true));
    assert_eq!(items[0]["nodeVersion"], json!("2"));
    let empty = Vec::new();
    let kinds: Vec<&str> = items[0]["cli"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|row| row["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"claude"), "{items:?}");
    assert!(kinds.contains(&"codex"), "{items:?}");
    assert!(kinds.contains(&"grok"), "{items:?}");
    drop(node);
    Ok(())
}

#[tokio::test]
async fn follow_tty_relays_snapshot_input_and_resize() -> Result<()> {
    use remuda_protocol::{BinaryChannel, StreamUuid, encode_binary_frame};

    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    let stream_id = remuda_protocol::Id::new("tty")?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "label": "tty-node" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "journal.append",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "event": { "kind": "lifecycle", "payload": { "status": "ready" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    let snapshot = b"\x1b[2J\x1b[Hsnapshot";
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "method": "tty.frame",
            "params": {
                "instanceId": instance_id.as_id().as_str(),
                "streamId": stream_id.as_str(),
                "channel": 1
            }
        })
        .to_string()
        .into(),
    ))
    .await?;

    let uuid = StreamUuid::from_prefixed_id(stream_id.as_str()).expect("stream uuid");
    let output = encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, snapshot)?;
    node.send(Message::Binary(output.clone().into())).await?;

    let mut follow_req = format!(
        "ws://{}/v1/follow?instanceId={}&tty=1",
        hub.addr,
        instance_id.as_id().as_str()
    )
    .into_client_request()?;
    follow_req
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());

    let node_task = tokio::spawn(async move {
        let mut saw_attach = false;
        let mut saw_write = false;
        let mut saw_resize = false;
        let mut node = node;
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(Ok(msg))) =
                tokio::time::timeout(Duration::from_millis(400), node.next()).await
            else {
                if saw_attach && saw_write && saw_resize {
                    break;
                }
                continue;
            };
            let Message::Text(text) = msg else { continue };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            if method == "tty.attach" {
                saw_attach = true;
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "streamId": stream_id.as_str(),
                        "streamEpoch": "epoch_01993ab0-0000-7000-8000-000000000099",
                        "availableFrom": "0",
                        "nextOffset": snapshot.len().to_string(),
                        "snapshotBase64": base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            snapshot
                        )
                    }
                });
                let _ = node.send(Message::Text(reply.to_string().into())).await;
            } else if method == "tty.write" {
                saw_write = true;
                let reply = json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}});
                let _ = node.send(Message::Text(reply.to_string().into())).await;
            } else if method == "tty.resize" {
                saw_resize = true;
                let cols = value.pointer("/params/cols").and_then(Value::as_u64);
                let rows = value.pointer("/params/rows").and_then(Value::as_u64);
                let reply = json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}});
                let _ = node.send(Message::Text(reply.to_string().into())).await;
                if cols == Some(100) && rows == Some(30) && saw_write && saw_attach {
                    return Ok::<_, anyhow::Error>((saw_attach, saw_write, saw_resize));
                }
            }
        }
        Ok((saw_attach, saw_write, saw_resize))
    });

    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req))
            .await
            .context("follow")??;

    let mut got_snapshot = false;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !got_snapshot {
        match tokio::time::timeout(Duration::from_millis(400), follow.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                assert_eq!(bytes[1], 1, "snapshot uses output channel");
                if bytes.windows(snapshot.len()).any(|w| w == snapshot) {
                    got_snapshot = true;
                }
            }
            Ok(Some(Ok(Message::Text(_)))) => continue,
            _ => continue,
        }
    }
    anyhow::ensure!(got_snapshot, "follow tty=1 did not replay snapshot first");

    let input = encode_binary_frame(BinaryChannel::TtyInput, uuid, 0, b"\x1b[<0;1;1M")?;
    follow.send(Message::Binary(input.into())).await?;
    follow
        .send(Message::Text(
            json!({ "type": "tty.resize", "cols": 100, "rows": 30 })
                .to_string()
                .into(),
        ))
        .await?;

    let (attach, write, resize) = tokio::time::timeout(TIMEOUT, node_task)
        .await
        .context("node task")???;
    anyhow::ensure!(attach, "hub did not relay tty.attach");
    anyhow::ensure!(write, "hub did not relay tty.write input");
    anyhow::ensure!(resize, "hub did not relay tty.resize");
    Ok(())
}

/// T2: `TtyRelay` is process-wide, so a binary frame is honoured only on the
/// socket that registered its stream. Host B must not be able to inject output
/// into an instance on host A by replaying A's stream UUID.
#[tokio::test]
async fn tty_binary_frames_are_scoped_to_the_registering_socket() -> Result<()> {
    use remuda_protocol::{BinaryChannel, StreamUuid, encode_binary_frame};

    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _) = login(hub.addr, &bootstrap).await?;
    let host_a = HostId::new();
    let instance_a = InstanceId::new();
    let stream_id = remuda_protocol::Id::new("tty")?;

    let enroll_a = enroll_token(hub.addr, &cookie).await?;
    let mut node_a = node_socket(hub.addr, &enroll_a, &host_a, "tty-host-a").await?;
    // Seed the instance so `tty.frame` passes the host-binding check.
    node_a
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "seed",
                "method": "journal.append",
                "params": {
                    "instanceId": instance_a.as_id().as_str(),
                    "event": { "kind": "lifecycle", "payload": { "status": "ready" } }
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let _ = recv_json(&mut node_a).await?;

    // Host A registers the stream for its own instance.
    node_a
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "bind",
                "method": "tty.frame",
                "params": {
                    "instanceId": instance_a.as_id().as_str(),
                    "streamId": stream_id.as_str(),
                    "channel": 1
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let ack = recv_json(&mut node_a).await?;
    anyhow::ensure!(ack["result"]["ok"] == json!(true), "{ack}");

    let mut follow_req = format!(
        "ws://{}/v1/follow?instanceId={}",
        hub.addr,
        instance_a.as_id().as_str()
    )
    .into_client_request()?;
    follow_req
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req)).await??;
    let snapshot = recv_json(&mut follow).await?;
    assert_eq!(snapshot["type"], json!("snapshot"));
    // Opt into binary delivery in-band; the query flag would trigger tty.attach.
    follow
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "instanceIds": [instance_a.as_id().as_str()],
                "tty": 1
            })
            .to_string()
            .into(),
        ))
        .await?;
    let resnapshot = recv_json(&mut follow).await?;
    assert_eq!(resnapshot["type"], json!("snapshot"));

    let attach = recv_json(&mut node_a).await?;
    assert_eq!(attach["method"], json!("tty.attach"));
    node_a
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": attach["id"],
                "result": { "streamId": stream_id.as_str(), "snapshotBase64": "" }
            })
            .to_string()
            .into(),
        ))
        .await?;

    let unknown_mode = recv_json(&mut follow).await?;
    assert_eq!(unknown_mode["type"], json!("tty.mode"));
    assert_eq!(unknown_mode["altScreen"], Value::Null);
    assert_eq!(unknown_mode["streamId"], json!(stream_id.as_str()));

    // Host B replays A's stream UUID on its own socket.
    let host_b = HostId::new();
    let enroll_b = enroll_token(hub.addr, &cookie).await?;
    let mut node_b = node_socket(hub.addr, &enroll_b, &host_b, "tty-host-b").await?;
    let uuid = StreamUuid::from_prefixed_id(stream_id.as_str()).expect("stream uuid");
    let forged = encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, b"forged")?;
    node_b.send(Message::Binary(forged.into())).await?;

    node_b.send(Message::Text(json!({
        "jsonrpc": "2.0", "id": "forged-mode", "method": "tty.mode",
        "params": { "instanceId": instance_a.as_id().as_str(), "streamId": stream_id.as_str(), "altScreen": true }
    }).to_string().into())).await?;
    let refused = recv_json(&mut node_b).await?;
    assert!(refused.get("error").is_some(), "{refused}");

    // Nothing must reach A's follower from B.
    let leaked = tokio::time::timeout(Duration::from_millis(400), follow.next()).await;
    anyhow::ensure!(
        leaked.is_err(),
        "host B must not publish into host A's instance: {leaked:?}"
    );

    // The owning socket still works.
    let genuine = encode_binary_frame(BinaryChannel::TtyOutput, uuid, 0, b"real")?;
    node_a.send(Message::Binary(genuine.into())).await?;
    let frame = tokio::time::timeout(TIMEOUT, follow.next())
        .await?
        .context("follow closed")??;
    match frame {
        Message::Binary(bytes) => assert!(bytes.ends_with(b"real"), "{bytes:?}"),
        Message::Text(text) => {
            let value: Value = serde_json::from_str(&text)?;
            assert_eq!(value["instanceId"], json!(instance_a.as_id().as_str()));
        }
        other => anyhow::bail!("unexpected follow frame {other:?}"),
    }
    // Renderer observations use the same host/instance/stream binding.
    for alt_screen in [false, true] {
        node_a.send(Message::Text(json!({
            "jsonrpc": "2.0", "id": "mode", "method": "tty.mode",
            "params": { "instanceId": instance_a.as_id().as_str(), "streamId": stream_id.as_str(), "altScreen": alt_screen }
        }).to_string().into())).await?;
        let ack = recv_json(&mut node_a).await?;
        assert_eq!(ack["result"]["ok"], json!(true), "{ack}");
        let observed = recv_json(&mut follow).await?;
        assert_eq!(observed["event"]["type"], json!("tty.mode"));
        assert_eq!(observed["event"]["params"]["altScreen"], json!(alt_screen));
        assert_eq!(
            observed["event"]["params"]["streamId"],
            json!(stream_id.as_str())
        );
    }
    let unknown_stream = remuda_protocol::Id::new("tty")?;
    node_a.send(Message::Text(json!({
        "jsonrpc": "2.0", "id": "unbound-mode", "method": "tty.mode",
        "params": { "instanceId": instance_a.as_id().as_str(), "streamId": unknown_stream.as_str(), "altScreen": true }
    }).to_string().into())).await?;
    assert!(recv_json(&mut node_a).await?.get("error").is_some());
    Ok(())
}

/// Open a `/v1/node` socket and complete `runtime.hello` for `host_id`.
async fn node_socket(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &HostId,
    label: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "label": label
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello.get("result").is_some(), "{hello}");
    Ok(node)
}

/// D-018: the device access code pairs devices only. It must not enroll a
/// Node. Repeat pairing under the same name is allowed on purpose — see the
/// note on `http::login`; the enforceable half of A2 is the TTL and rotation.
#[tokio::test]
async fn access_code_pairs_devices_but_never_enrolls_a_node() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _) = login(hub.addr, &bootstrap).await?;

    // A repeat login under the same device name still succeeds: `deviceName` is
    // unauthenticated, so refusing it would only break honest repeat clients
    // (CLI, `hub --with-dispatcher`) without stopping an attacker.
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "test-phone" }).to_string();
    let (status, _, _) = http(hub.addr, "POST", "/v1/login", &[], Some(&body)).await?;
    assert_eq!(status, 200, "repeat pairing under the same name must work");

    // The access code must not authenticate a node socket.
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("node connect")??;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "node.hello",
            "params": { "hostId": HostId::new().as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let denied = recv_json(&mut node).await?;
    assert_eq!(
        denied["error"]["code"],
        json!(-32000),
        "device access code must not enroll a Node: {denied}"
    );

    // A minted enroll token does enroll, exactly once.
    let enroll = enroll_token(hub.addr, &cookie).await?;
    let enrolled_host = HostId::new();
    let node = node_socket(hub.addr, &enroll, &enrolled_host, "enrolled").await?;
    drop(node);
    let host_id = enrolled_host.as_id().as_str().to_string();

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut replay, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("replay connect")??;
    replay
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": "r",
                "method": "node.hello",
                "params": { "hostId": host_id, "nodeVersion": "0.1.0" }
            })
            .to_string()
            .into(),
        ))
        .await?;
    let denied = recv_json(&mut replay).await?;
    assert_eq!(
        denied["error"]["code"],
        json!(-32000),
        "enroll token must be single use: {denied}"
    );
    Ok(())
}

/// Regression: two clients sharing a hardcoded `deviceName` (the CLI and the
/// `hub --with-dispatcher` combined mode both use `remuda-hub-client`) must
/// both authenticate. A one-shot-per-name rule broke this with a 401.
#[tokio::test]
async fn repeat_pairing_under_a_shared_device_name_succeeds() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    for attempt in 0..3 {
        let body =
            json!({ "bootstrapToken": bootstrap, "deviceName": "remuda-hub-client" }).to_string();
        let (status, head, rest) = http(hub.addr, "POST", "/v1/login", &[], Some(&body)).await?;
        assert_eq!(status, 200, "attempt {attempt}: {rest}");
        let cookie = cookie_from(&head).context("set-cookie")?;
        // Each login mints a *distinct*, independently usable device token.
        let (status, _, hosts) = http(
            hub.addr,
            "GET",
            "/v1/hosts",
            &[("Cookie", cookie.as_str())],
            None,
        )
        .await?;
        assert_eq!(status, 200, "attempt {attempt}: {hosts}");
    }
    Ok(())
}

/// A device token is not an enroll token: minting requires authentication.
#[tokio::test]
async fn enroll_token_requires_an_authenticated_device() -> Result<()> {
    let (hub, _bootstrap, _dir) = boot().await?;
    let (status, _, _) = http(hub.addr, "POST", "/v1/hosts/enroll-token", &[], Some("{}")).await?;
    assert_eq!(status, 401);
    Ok(())
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
