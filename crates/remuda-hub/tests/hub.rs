//! Drive Hub HTTP + `/v1/node` + `/v1/follow` with a fake Node client.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// model-pin-1 §5.4 public-API regression: a launch `model_pin_mismatch`
/// diagnostic the Node appends is projected onto the instance and served
/// verbatim on the public GET instance JSON, so run details keeps it even
/// when the launch event is older than the bounded journal tail.
#[tokio::test]
async fn model_pin_mismatch_is_served_on_public_instance_json() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "h",
            "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0", "label": "modelpin-api" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    // Accept every RPC the Hub issues (create/send/…); after the test sends
    // the diagnostic event on this channel, the handler appends it onto the
    // (already-created) instance.
    let (append_tx, append_rx) = tokio::sync::oneshot::channel::<(String, Value)>();
    tokio::spawn(async move {
        let mut pending: Option<(String, Value)> = append_rx.await.ok();
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let method = frame["method"].as_str().unwrap_or_default().to_owned();
            let command_id = frame
                .pointer("/params/commandId")
                .cloned()
                .unwrap_or_else(|| json!("cmd_pin"));
            let reply = json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "command": { "commandId": command_id, "state": "accepted", "operation": method } }
            });
            let _ = node.send(Message::Text(reply.to_string().into())).await;
            if let Some((instance_id, event)) = pending.take() {
                let _ = node
                    .send(Message::Text(
                        json!({
                            "jsonrpc": "2.0", "id": "pin",
                            "method": "journal.append",
                            "params": { "instanceId": instance_id, "event": event }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
            }
        }
    });

    // Create the instance over the public HTTP API (pins model A).
    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "model": "model_hub/es1_orange_o50[1m]",
        "prompt": "pin probe"
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

    // Queue the Node's launch mismatch diagnostic for append on the live
    // Node link (the same channel the production pump uses). The background
    // handler sends it right after accepting the create RPC.
    let pin_event: Value = serde_json::from_str(
        &json!({
            "kind": "lifecycle",
            "observedAt": "2026-09-24T00:00:00.000Z",
            "payload": {
                "type": "native",
                "topic": "diagnostic",
                "nativeName": "model_pin_mismatch",
                "nativeId": { "state": "not-applicable" },
                "status": { "state": "known", "value": "diverged" },
                "severity": "warning",
                "affectsCompletion": false,
                "dataRef": null,
                "relatedIds": {
                    "reason": "model-mismatch",
                    "requested": "model_hub/es1_orange_o50[1m]",
                    "observed": "model_hub/es1_orange_o48[1m]"
                }
            }
        })
        .to_string(),
    )?;
    append_tx
        .send((instance_id.clone(), pin_event))
        .map_err(|_| anyhow!("append channel closed"))?;
    // The background accepter owns the socket now; the append is persisted on
    // arrival regardless of when its JSON-RPC reply is drained. Poll the
    // public record until the projection lands.

    // Read it back through the PUBLIC instance endpoint, polling briefly until
    // the append's projection lands, and assert the exact projected JSON
    // values — not private store internals.
    let inst: Value = tokio::time::timeout(TIMEOUT, async {
        loop {
            let (status, _, body) = http(
                hub.addr,
                "GET",
                &format!("/v1/instances/{instance_id}"),
                &[("Cookie", &cookie)],
                None,
            )
            .await?;
            assert_eq!(status, 200, "{body}");
            let inst: Value = serde_json::from_str(body.trim())?;
            if inst.get("modelPinMismatches").is_some() {
                return Ok::<_, anyhow::Error>(inst);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("projection did not reach the public instance record")??;
    let records = inst["modelPinMismatches"]
        .as_array()
        .context("modelPinMismatches array on public record")?;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["requested"].as_str(),
        Some("model_hub/es1_orange_o50[1m]")
    );
    assert_eq!(
        records[0]["observed"].as_str(),
        Some("model_hub/es1_orange_o48[1m]")
    );
    assert_eq!(
        records[0]["observedAt"].as_str(),
        Some("2026-09-24T00:00:00.000Z")
    );
    hub.shutdown().await;
    Ok(())
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
async fn a_send_the_node_rejects_settles_rejected_and_is_listed_by_the_new_route() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
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
    // invalid-request error, so the Hub must settle the row with a `rejected`
    // outcome rather than leave it queued (docs/design/evidence/instance-send-1.md).
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
        json!("settled"),
        "a rejected send settles with a rejected outcome, it is not a 4th state: {body}"
    );
    assert_eq!(body["command"]["resolution"], json!("clear"), "{body}");
    assert_eq!(
        body["command"]["settlement"]["outcome"],
        json!("rejected"),
        "{body}"
    );
    let reason = body["command"]["settlement"]["reason"]
        .as_str()
        .context("rejection reason present")?;
    assert!(
        reason.contains("input.text") || reason.contains("input.blocks"),
        "the reason carries the node's message: {reason}"
    );
    assert!(
        body["command"].get("reason").is_none(),
        "no top-level reason field — the reason rides the settlement: {body}"
    );

    // The new GET route lists the command, newest first, with its settlement.
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
    assert_eq!(row["state"], json!("settled"));
    assert_eq!(row["resolution"], json!("clear"));
    assert_eq!(row["settlement"]["outcome"], json!("rejected"));
    assert!(
        row["settlement"]
            .get("reason")
            .and_then(Value::as_str)
            .is_some(),
        "{row}"
    );
    assert!(row.get("reason").is_none(), "no top-level reason: {row}");
    assert!(row.get("createdAt").is_some() && row.get("updatedAt").is_some());
    Ok(())
}

/// The Node *receives* `instance.send` but never replies, so the RPC times out
/// and the Hub marks the row `reconciling` (§2.5). Crucially the Hub must **not**
/// settle it on its own timer: a forward intent exists, so without evidence the
/// command did not run it cannot be called `rejected` (§2.5, §12.2), and there
/// is no fourth state. The row rests at `queued` / `reconciling`, forwarded,
/// with no settlement, until the Node's journal says otherwise. The node task
/// below answers `runtime.hello` and `instance.create` but silently drops
/// `instance.send`.
#[tokio::test]
async fn a_send_the_node_never_acks_rests_reconciling_and_is_never_self_failed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    // A short accept timeout so the RPC gives up quickly.
    config.command_accept_timeout_ms = 200;
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
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0", "label": "silent-send" }
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
            let method = frame["method"].as_str().unwrap_or_default().to_owned();
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            let command_id = params
                .get("commandId")
                .cloned()
                .unwrap_or_else(|| json!("cmd_test"));
            // Receive instance.send but never reply — the lost-reply path.
            // Everything else is accepted so the instance is reachable.
            if method == "instance.send" {
                continue;
            }
            let _ = node
                .send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "command": { "commandId": command_id, "state": "accepted", "operation": method }
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
    // The lost RPC reply marked it reconciling, still three-state `queued`.
    assert_eq!(body["command"]["state"], json!("queued"), "{body}");
    assert_eq!(
        body["command"]["resolution"],
        json!("reconciling"),
        "{body}"
    );
    assert_eq!(body["command"]["forwarded"], json!(true), "{body}");
    assert!(body["command"].get("settlement").is_none(), "{body}");

    // Well past where the old ack deadline (400 ms) would have fired, the row is
    // unchanged: the Hub never self-settles a forwarded command it cannot prove.
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(100)).await;
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
        let row = listed["commands"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["commandId"].as_str() == Some(command_id.as_str()))
            })
            .context("send command listed")?;
        assert!(
            matches!(
                row["state"].as_str(),
                Some("queued" | "accepted" | "settled")
            ),
            "never a fourth command state, got {}",
            row["state"]
        );
        assert_ne!(
            row["state"],
            json!("settled"),
            "a never-acked send must not be settled (rejected or otherwise): {row}"
        );
        assert!(
            row.get("settlement").is_none(),
            "no settlement is invented: {row}"
        );
        assert!(
            row.get("reason").is_none(),
            "no top-level reason field: {row}"
        );
        assert_eq!(row["forwarded"], json!(true), "{row}");
    }
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

/// A fake Node socket after `runtime.hello`: the concrete stream type the
/// `/v1/node` client negotiates.
type FakeNode = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// Connect `/v1/node`, complete `runtime.hello`, and return the socket plus the
/// durable `nodeToken` the hello hands back (used to reconnect).
async fn open_fake_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &HostId,
    label: &str,
) -> Result<(FakeNode, String)> {
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("fake node connect")??;
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
    let node_token = hello["result"]["nodeToken"]
        .as_str()
        .unwrap_or(bearer)
        .to_string();
    Ok((node, node_token))
}

/// Drive a connected fake Node: answer every RPC `accepted`, count inbound
/// `instance.send` frames, and mirror one `message` journal event per send so
/// journal-level exactly-once can be asserted. Abort the returned task to drop
/// the socket and take the host offline.
fn drive_accepting_node(node: FakeNode, sends: Arc<AtomicUsize>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut node = node;
        let mut mirrored = 0u64;
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
            if method == "instance.send" {
                sends.fetch_add(1, Ordering::Relaxed);
                if let (Some(instance_id), Some(text)) =
                    (params["instanceId"].as_str(), params["text"].as_str())
                {
                    mirrored += 1;
                    let append = json!({
                        "jsonrpc": "2.0",
                        "id": format!("mirror-{mirrored}"),
                        "method": "journal.append",
                        "params": {
                            "instanceId": instance_id,
                            "event": {
                                "schemaVersion": 1,
                                "kind": "message",
                                "completeness": "structured",
                                "payload": { "role": "user", "text": text }
                            }
                        }
                    });
                    let _ = node.send(Message::Text(append.to_string().into())).await;
                }
            }
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "command": {
                        "commandId": command_id,
                        "state": "accepted",
                        "operation": method
                    }
                }
            });
            let _ = node.send(Message::Text(reply.to_string().into())).await;
        }
    })
}

/// Hello an accepting fake Node and start its driver. Returns the reconnect
/// token and the driver task.
async fn accepting_node(
    addr: std::net::SocketAddr,
    bearer: &str,
    host_id: &HostId,
    label: &str,
    sends: Arc<AtomicUsize>,
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let (node, node_token) = open_fake_node(addr, bearer, host_id, label).await?;
    Ok((node_token, drive_accepting_node(node, sends)))
}

/// Create a `claude-print` instance on `host_id` via HTTP and return its id.
async fn create_print_instance(
    addr: std::net::SocketAddr,
    cookie: &str,
    host_id: &str,
) -> Result<String> {
    let create = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "prompt": "hi"
    })
    .to_string();
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie)],
        Some(&create),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    Ok(body["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string())
}

/// G1/G2: posting the same client commandId three times forwards the command
/// exactly once — the first POST creates and forwards, both replays return the
/// original row — and the Node mirrors exactly one message into the journal.
#[tokio::test]
async fn same_command_id_posted_three_times_forwards_and_journals_once() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "replay-once", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "deliver once" }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let mut replayed = Vec::new();
    for _ in 0..3 {
        let (status, _, resp) =
            http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
        assert_eq!(status, 200, "{resp}");
        let resp: Value = serde_json::from_str(resp.trim())?;
        replayed.push(resp["replayed"].as_bool().context("replayed")?);
    }
    assert_eq!(
        replayed,
        vec![false, true, true],
        "first POST creates, replays do not"
    );
    // Let any wrongly-sent trailing frame arrive before counting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "Node must see instance.send once"
    );

    let (status, _, journal) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}/journal"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{journal}");
    let journal: Value = serde_json::from_str(journal.trim())?;
    let messages = journal["events"]
        .as_array()
        .context("events")?
        .iter()
        .filter(|row| row["event"]["kind"].as_str() == Some("message"))
        .count();
    assert_eq!(messages, 1, "exactly one message in the journal: {journal}");
    Ok(())
}

/// G1: a command queued while the host is offline must NOT auto-forward on
/// reconnect, but a same-id re-POST once the host is back forwards it exactly
/// once.
#[tokio::test]
async fn offline_queued_command_forwards_only_when_same_id_is_reposted() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (node_token, link) =
        accepting_node(hub.addr, &enroll, &host_id, "g1-before", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    link.abort();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "queued while offline" }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, queued) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{queued}");
    let queued: Value = serde_json::from_str(queued.trim())?;
    assert_eq!(queued["command"]["state"], json!("queued"));
    assert_eq!(queued["command"]["forwarded"], json!(false));

    // Reconnect: the Hub must not replay anything by itself.
    let (node, _) = open_fake_node(hub.addr, &node_token, &host_id, "g1-after").await?;
    let sends_after = Arc::new(AtomicUsize::new(0));
    let _link = drive_accepting_node(node, sends_after.clone());
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), true).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sends_after.load(Ordering::Relaxed),
        0,
        "reconnect alone must not replay commands"
    );

    // Same id re-POST forwards the queued row, once.
    let (status, _, replay) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{replay}");
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(replay["command"]["forwarded"], json!(true), "{replay}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sends_after.load(Ordering::Relaxed), 1);
    assert_eq!(
        sends.load(Ordering::Relaxed),
        0,
        "the pre-reconnect node saw nothing"
    );

    // The status endpoint shows the converged row.
    let (status, _, status_body) = http(
        hub.addr,
        "GET",
        &format!("{path}/{}", command_id.as_id().as_str()),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{status_body}");
    let status_row: Value = serde_json::from_str(status_body.trim())?;
    assert_eq!(status_row["state"], json!("accepted"), "{status_row}");
    assert_eq!(status_row["forwarded"], json!(true));
    Ok(())
}

/// G1 guard: two concurrent same-id POSTs result in one Node dispatch — one
/// caller creates, the other replays, and only one wins `mark_forward_intent`.
#[tokio::test]
async fn concurrent_same_id_reposts_forward_once() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "g1-race", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "race once" }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let one = {
        let addr = hub.addr;
        let cookie = cookie.clone();
        let path = path.clone();
        let body = body.clone();
        tokio::spawn(
            async move { http(addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await },
        )
    };
    let two = {
        let addr = hub.addr;
        let cookie = cookie.clone();
        tokio::spawn(
            async move { http(addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await },
        )
    };
    let (first, second) = tokio::join!(one, two);
    let (status_a, _, body_a) = first??;
    let (status_b, _, body_b) = second??;
    assert_eq!(status_a, 200, "{body_a}");
    assert_eq!(status_b, 200, "{body_b}");
    let replayed = [body_a, body_b]
        .iter()
        .map(|raw| {
            serde_json::from_str::<Value>(raw.trim())
                .and_then(|value| {
                    value["replayed"]
                        .as_bool()
                        .ok_or(serde::de::Error::custom("replayed"))
                })
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    assert!(
        replayed.contains(&true) && replayed.contains(&false),
        "one POST creates, the other replays: {replayed:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "racing replays dispatch once"
    );
    Ok(())
}

/// G3: after a send completes, expiring its attachment must not turn a same-id
/// re-POST into a 400 — dedup precedes attachment resolution, and the retry's
/// raw object references are compared against the stored resolved manifest.
/// A NEW send referencing the expired object still fails at the boundary.
#[tokio::test]
async fn same_id_repost_after_attachment_expiry_replays() -> Result<()> {
    let (hub, bootstrap, dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "g3-file", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let (status, _, uploaded) = http(
        hub.addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", &cookie)],
        Some("hello attachment"),
    )
    .await?;
    assert_eq!(status, 200, "{uploaded}");
    let uploaded: Value = serde_json::from_str(uploaded.trim())?;
    let object_id = uploaded["objectId"].as_str().context("objectId")?;

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": {
            "text": "carries a file",
            "attachments": [{ "objectId": object_id }]
        }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, first) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{first}");
    let first: Value = serde_json::from_str(first.trim())?;
    assert_eq!(first["replayed"], json!(false));
    assert_eq!(
        first["command"]["payload"]["attachments"][0]["objectId"],
        json!(object_id)
    );

    // Expire the object underneath the already-delivered command.
    let db = rusqlite::Connection::open(dir.path().join("data").join("hub.sqlite"))?;
    db.execute(
        "UPDATE objects SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![object_id],
    )?;

    // Same id, same raw request: replay, not a 400.
    let (status, _, replay) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(
        status, 200,
        "expired attachment must not break replay: {replay}"
    );
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay["replayed"], json!(true));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "replay must not redispatch"
    );

    // A fresh send with the expired object is still rejected at POST time.
    let late = json!({
        "operation": "instance.send",
        "payload": {
            "text": "late file",
            "attachments": [{ "objectId": object_id }]
        }
    })
    .to_string();
    let (status, _, rejected) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&late)).await?;
    assert_eq!(status, 400, "{rejected}");
    let rejected: Value = serde_json::from_str(rejected.trim())?;
    assert_eq!(rejected["code"], json!("BAD_REQUEST"), "{rejected}");
    assert_eq!(sends.load(Ordering::Relaxed), 1);
    Ok(())
}

/// Same commandId, different payload is still `COMMAND_ID_CONFLICT` (409).
#[tokio::test]
async fn same_command_id_with_different_payload_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "g3-conflict", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let command_id = remuda_protocol::CommandId::new();
    let path = format!("/v1/instances/{instance_id}/commands");
    let first = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "first message" }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&first),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    let second = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "different message" }
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&second),
    )
    .await?;
    assert_eq!(status, 409, "{conflict}");
    let conflict: Value = serde_json::from_str(conflict.trim())?;
    assert_eq!(conflict["code"], json!("COMMAND_ID_CONFLICT"), "{conflict}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "the conflicting retry never forwards"
    );
    Ok(())
}

/// A commandId the Node would reject is refused with 400 at the Hub boundary;
/// the Node never sees it.
#[tokio::test]
async fn malformed_command_id_is_rejected_with_400() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "bad-id", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    let valid = remuda_protocol::CommandId::new();
    let valid_str = valid.as_id().as_str();
    let uppercase_uuid = format!("cmd_{}", valid_str[4..].to_uppercase());
    let other_brand = InstanceId::new();
    let bad_ids = [
        "not-an-id",
        "cmd_bad",
        uppercase_uuid.as_str(),
        // Canonical UUIDv7 but a different entity brand.
        other_brand.as_id().as_str(),
    ];
    for bad_id in bad_ids {
        let body = json!({
            "commandId": bad_id,
            "operation": "instance.send",
            "payload": { "text": "bad id" }
        })
        .to_string();
        let (status, _, rejected) =
            http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
        assert_eq!(status, 400, "{bad_id} must be 400, got: {rejected}");
        let rejected: Value = serde_json::from_str(rejected.trim())?;
        assert_eq!(rejected["code"], json!("BAD_REQUEST"), "{rejected}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        0,
        "a malformed id never reaches the Node"
    );
    Ok(())
}

/// G2: the command status endpoint returns the same row POST returned, hides
/// other instances' rows behind 404, refuses Agent credentials, and requires
/// authentication.
#[tokio::test]
async fn command_status_endpoint_returns_row_and_enforces_principals() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "g2-get", sends.clone()).await?;
    let instance_a = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let instance_b = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "status check" }
    })
    .to_string();
    let (status, _, posted) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{instance_a}/commands"),
        &[("Cookie", &cookie)],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 200, "{posted}");
    let posted: Value = serde_json::from_str(posted.trim())?;

    // The GET row is the POST row.
    let (status, _, row) = http(
        hub.addr,
        "GET",
        &format!(
            "/v1/instances/{instance_a}/commands/{}",
            command_id.as_id().as_str()
        ),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{row}");
    let row: Value = serde_json::from_str(row.trim())?;
    assert_eq!(
        row, posted["command"],
        "GET row equals the POSTed command row"
    );

    // Unknown command id on a real instance is 404.
    let other = remuda_protocol::CommandId::new();
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!(
            "/v1/instances/{instance_a}/commands/{}",
            other.as_id().as_str()
        ),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 404);

    // The same row is not reachable through another instance's path.
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!(
            "/v1/instances/{instance_b}/commands/{}",
            command_id.as_id().as_str()
        ),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 404, "cross-instance command read must not leak");

    // Agent credentials — bound to the target, to a sibling, or to an
    // unrelated instance — cannot read commands. A commands detail path is not
    // an Agent read route in the middleware, and the handler re-checks, so an
    // Agent has no command-status visibility at all.
    let instance_c = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    for bound in [&instance_a, &instance_b, &instance_c] {
        let name = format!("agent-{bound}");
        let token = hub.test_mint_agent_token(&name, bound).await?;
        let (status, _, forbidden) = http(
            hub.addr,
            "GET",
            &format!(
                "/v1/instances/{instance_a}/commands/{}",
                command_id.as_id().as_str()
            ),
            &[("Authorization", &format!("Bearer {token}"))],
            None,
        )
        .await?;
        assert_eq!(
            status, 403,
            "agent bound to {bound} must not read: {forbidden}"
        );
    }

    // A SECOND, separately authenticated Human device paired to the same owner
    // intentionally shares visibility: Human/Bot seats are universe roots, and
    // command reads follow exactly the same rule as journal reads — the row is
    // scoped to the instance, not to the submitting device.
    let (cookie2, _) = login(hub.addr, &bootstrap).await?;
    let (status, _, shared) = http(
        hub.addr,
        "GET",
        &format!(
            "/v1/instances/{instance_a}/commands/{}",
            command_id.as_id().as_str()
        ),
        &[("Cookie", &cookie2)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{shared}");
    let shared: Value = serde_json::from_str(shared.trim())?;
    assert_eq!(
        shared, posted["command"],
        "same-owner device sees the same row"
    );
    // Parity: the second device can read the instance journal too.
    let (status, _, journal) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_a}/journal"),
        &[("Cookie", &cookie2)],
        None,
    )
    .await?;
    assert_eq!(
        status, 200,
        "command visibility must match journal visibility: {journal}"
    );

    // No credential at all: 401.
    let (status, _, _) = http(
        hub.addr,
        "GET",
        &format!(
            "/v1/instances/{instance_a}/commands/{}",
            command_id.as_id().as_str()
        ),
        &[],
        None,
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

/// Replay equality covers every Node-executed prompt shape. Reusing a
/// commandId with a different `input.text`, a different string `input`, or
/// different `input.blocks` text is a 409 — the new content must never be
/// silently dropped behind `replayed:true`.
#[tokio::test]
async fn same_command_id_with_different_nested_prompt_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r2-prompt", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    // One accepted row per shape, then a same-id retry carrying changed text.
    let cases = [
        (
            json!({ "input": { "text": "alpha" } }),
            json!({ "input": { "text": "beta" } }),
        ),
        (
            json!({ "input": "gamma string" }),
            json!({ "input": "delta string" }),
        ),
        (
            json!({ "input": { "type": "prompt", "blocks": [{ "type": "text", "text": "block alpha" }] } }),
            json!({ "input": { "type": "prompt", "blocks": [{ "type": "text", "text": "block beta" }] } }),
        ),
        (
            json!({ "text": "flat alpha" }),
            json!({ "text": "flat beta" }),
        ),
        (
            json!({ "prompt": "field alpha" }),
            json!({ "prompt": "field beta" }),
        ),
    ];
    let case_count = cases.len();
    for (first_payload, changed_payload) in cases {
        let command_id = remuda_protocol::CommandId::new();
        let first = json!({
            "commandId": command_id.as_id().as_str(),
            "operation": "instance.send",
            "payload": first_payload
        })
        .to_string();
        let (status, _, body) = http(
            hub.addr,
            "POST",
            &path,
            &[("Cookie", &cookie)],
            Some(&first),
        )
        .await?;
        assert_eq!(status, 200, "{body}");

        let changed = json!({
            "commandId": command_id.as_id().as_str(),
            "operation": "instance.send",
            "payload": changed_payload
        })
        .to_string();
        let (status, _, conflict) = http(
            hub.addr,
            "POST",
            &path,
            &[("Cookie", &cookie)],
            Some(&changed),
        )
        .await?;
        assert_eq!(
            status,
            409,
            "changed nested prompt must conflict for id {}",
            command_id.as_id().as_str()
        );
        let conflict: Value = serde_json::from_str(conflict.trim())?;
        assert_eq!(conflict["code"], json!("COMMAND_ID_CONFLICT"));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        case_count,
        "only the {case_count} first POSTs dispatch"
    );
    Ok(())
}

/// Delivery mode is executable: a same-id retry changing `input.mode` or the
/// flattened `mode` is a 409.
#[tokio::test]
async fn same_command_id_with_different_mode_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r2-mode", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    // Nested mode (the CLI/MCP shape).
    let nested_id = remuda_protocol::CommandId::new();
    let nested = json!({
        "commandId": nested_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "input": {
            "type": "prompt",
            "mode": "steer",
            "blocks": [{ "type": "text", "text": "steer me" }]
        } }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&nested),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let changed = json!({
        "commandId": nested_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "input": {
            "type": "prompt",
            "mode": "queue",
            "blocks": [{ "type": "text", "text": "steer me" }]
        } }
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&changed),
    )
    .await?;
    assert_eq!(status, 409, "changed input.mode must conflict: {conflict}");

    // Flattened mode.
    let flat_id = remuda_protocol::CommandId::new();
    let flat = json!({
        "commandId": flat_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "flat mode", "mode": "steer" }
    })
    .to_string();
    let (status, _, body) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&flat)).await?;
    assert_eq!(status, 200, "{body}");
    let changed = json!({
        "commandId": flat_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "flat mode", "mode": "new-turn" }
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&changed),
    )
    .await?;
    assert_eq!(
        status, 409,
        "changed flattened mode must conflict: {conflict}"
    );

    // Identical nested replay still works.
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&nested),
    )
    .await?;
    assert_eq!(status, 200, "{replay}");
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay["replayed"], json!(true));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        2,
        "conflicting retries never dispatch"
    );
    Ok(())
}

/// Provenance is executable: a commandId first POSTed by a Human cannot be
/// replayed by an Agent credential. The stamped origins differ, so the retry
/// is a 409 both when the stored row has already been accepted and when it is
/// still queued offline — including an attachment-bearing retry, which must
/// not bypass the Agent attachment prohibition.
#[tokio::test]
async fn human_command_id_replayed_by_agent_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (node_token, link) =
        accepting_node(hub.addr, &enroll, &host_id, "r2-origin", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");
    let agent_name = format!("agent-{instance_id}");
    let agent = hub.test_mint_agent_token(&agent_name, &instance_id).await?;
    let agent_auth = format!("Bearer {agent}");

    // Accepted row: Human posts, Agent replays the same id and text.
    let accepted_id = remuda_protocol::CommandId::new();
    let accepted_body = json!({
        "commandId": accepted_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "human says hello" }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&accepted_body),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Authorization", &agent_auth)],
        Some(&accepted_body),
    )
    .await?;
    assert_eq!(
        status, 409,
        "agent-stamped replay of a human row must conflict: {conflict}"
    );
    let conflict: Value = serde_json::from_str(conflict.trim())?;
    assert_eq!(conflict["code"], json!("COMMAND_ID_CONFLICT"));

    // Queued row, attachment-bearing: host goes offline before the Human POST.
    link.abort();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;
    let (status, _, uploaded) = http(
        hub.addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", &cookie)],
        Some("origin attachment"),
    )
    .await?;
    assert_eq!(status, 200, "{uploaded}");
    let object_id = serde_json::from_str::<Value>(uploaded.trim())?["objectId"]
        .as_str()
        .unwrap()
        .to_string();
    let queued_id = remuda_protocol::CommandId::new();
    let queued_body = json!({
        "commandId": queued_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": {
            "text": "human with file",
            "attachments": [{ "objectId": object_id }]
        }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&queued_body),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(body.trim())?;
    assert_eq!(body["command"]["forwarded"], json!(false));

    // The Agent replay conflicts on origin BEFORE attachment resolution and
    // before forwarding, even though the host is still offline.
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Authorization", &agent_auth)],
        Some(&queued_body),
    )
    .await?;
    assert_eq!(status, 409, "{conflict}");
    let conflict: Value = serde_json::from_str(conflict.trim())?;
    assert_eq!(conflict["code"], json!("COMMAND_ID_CONFLICT"));
    // The queued row was not forwarded by the rejected retry.
    let (status, _, row) = http(
        hub.addr,
        "GET",
        &format!("{path}/{}", queued_id.as_id().as_str()),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{row}");
    let row: Value = serde_json::from_str(row.trim())?;
    assert_eq!(row["forwarded"], json!(false), "{row}");

    // Reconnect and prove the queued Human command still forwards exactly once
    // under a Human retry (no stale Agent intent stuck it).
    let (node, _) = open_fake_node(hub.addr, &node_token, &host_id, "r2-origin-back").await?;
    let sends_back = Arc::new(AtomicUsize::new(0));
    let _link = drive_accepting_node(node, sends_back.clone());
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), true).await?;
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&queued_body),
    )
    .await?;
    assert_eq!(status, 200, "{replay}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sends_back.load(Ordering::Relaxed), 1);
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "only the pre-offline Human send was seen before"
    );
    Ok(())
}

/// G3 normalization: an attachment id accepted with surrounding whitespace
/// (the first-POST validator trims it) still replays byte-identically — before
/// AND after the object expires — without a second dispatch.
#[tokio::test]
async fn same_id_replay_with_whitespace_attachment_id_matches() -> Result<()> {
    let (hub, bootstrap, dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r3-trim", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let (status, _, uploaded) = http(
        hub.addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", &cookie)],
        Some("trimmed attachment"),
    )
    .await?;
    assert_eq!(status, 200, "{uploaded}");
    let object_id = serde_json::from_str::<Value>(uploaded.trim())?["objectId"]
        .as_str()
        .unwrap()
        .to_string();
    let padded_id = format!("  {object_id} ");

    let command_id = remuda_protocol::CommandId::new();
    // The identical raw body is what a client would retransmit.
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": {
            "text": "padded file",
            "attachments": [{ "objectId": padded_id }]
        }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, first) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{first}");
    let first: Value = serde_json::from_str(first.trim())?;
    assert_eq!(first["replayed"], json!(false));
    assert_eq!(
        first["command"]["payload"]["attachments"][0]["objectId"],
        json!(object_id),
        "the stored manifest carries the trimmed id"
    );

    // Byte-identical retransmit while fresh: replay, not conflict.
    let (status, _, replay) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "whitespace-padded id must replay: {replay}");
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay["replayed"], json!(true));

    // Same after expiry.
    let db = rusqlite::Connection::open(dir.path().join("data").join("hub.sqlite"))?;
    db.execute(
        "UPDATE objects SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![object_id],
    )?;
    let (status, _, replay) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(
        status, 200,
        "expired padded attachment still replays: {replay}"
    );
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay["replayed"], json!(true));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "replays must not redispatch"
    );
    Ok(())
}

/// A forward intent marked just as the Node link disappears must be released:
/// the frame was never queued, so after the host returns a same-id re-POST
/// forwards the command exactly once instead of sticking at `forwarded=1`.
#[tokio::test]
async fn forward_intent_is_released_when_the_frame_was_never_queued() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (node_token, link) =
        accepting_node(hub.addr, &enroll, &host_id, "r2-stuck", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    // Queue a same-id command while the host is offline.
    link.abort();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;
    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "stuck intent" }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, queued) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{queued}");
    let queued: Value = serde_json::from_str(queued.trim())?;
    assert_eq!(queued["command"]["forwarded"], json!(false));

    // A "live" link that nevertheless refuses before the frame is queued:
    // kind_of sees a transport, but every call returns Ok(None). The host row
    // from the real enrollment is left intact so its nodeToken still
    // authenticates the reconnect below.
    hub.test_set_node_reply(host_id.as_id().as_str(), None)
        .await;
    let (status, _, attempt) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{attempt}");
    let attempt: Value = serde_json::from_str(attempt.trim())?;
    assert_eq!(attempt["replayed"], json!(true));
    assert_eq!(attempt["command"]["state"], json!("queued"), "{attempt}");
    assert_eq!(
        attempt["command"]["forwarded"],
        json!(false),
        "intent must roll back when the frame never queued: {attempt}"
    );
    assert_eq!(
        attempt["command"]["resolution"],
        json!("clear"),
        "{attempt}"
    );

    // The real Node reconnects; the next same-id re-POST forwards once.
    let (node, _) = open_fake_node(hub.addr, &node_token, &host_id, "r2-stuck-back").await?;
    let sends_back = Arc::new(AtomicUsize::new(0));
    let _link = drive_accepting_node(node, sends_back.clone());
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), true).await?;
    let (status, _, delivered) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{delivered}");
    let delivered: Value = serde_json::from_str(delivered.trim())?;
    assert_eq!(delivered["replayed"], json!(true));
    assert_eq!(delivered["command"]["forwarded"], json!(true));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends_back.load(Ordering::Relaxed),
        1,
        "the reconnected Node receives the send exactly once"
    );
    assert_eq!(
        sends.load(Ordering::Relaxed),
        0,
        "the pre-offline Node saw nothing"
    );

    // One more retry after delivery must not dispatch again.
    let (status, _, again) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{again}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sends_back.load(Ordering::Relaxed), 1);
    Ok(())
}

/// Executable nested attachment metadata is compared verbatim: changing
/// `kind` or `digest` inside `input.attachments` under the same commandId is a
/// 409, both for an accepted row and for a queued row whose host was offline.
#[tokio::test]
async fn same_command_id_changed_nested_attachment_metadata_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r3-nested", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");
    // Nested attachments are never rewritten by the Hub, so no upload is
    // needed: the fake Node accepts whatever metadata the frame carries.
    let post = |kind: &'static str, digest: &'static str| {
        json!({
            "input": {
                "text": "inspect this",
                "attachments": [{
                    "objectId": "obj_nested_fixture",
                    "kind": kind,
                    "mediaType": "image/png",
                    "digest": digest
                }]
            }
        })
    };
    let post_command = |command_id: &remuda_protocol::CommandId, payload: Value| {
        let body = json!({
            "commandId": command_id.as_id().as_str(),
            "operation": "instance.send",
            "payload": payload
        })
        .to_string();
        let addr = hub.addr;
        let cookie = cookie.clone();
        let path = path.clone();
        async move { http(addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await }
    };

    // Accepted row: image -> file must conflict.
    let image_id = remuda_protocol::CommandId::new();
    let (status, _, body) = post_command(&image_id, post("image", "digest-a")).await?;
    assert_eq!(status, 200, "{body}");
    let (status, _, conflict) = post_command(&image_id, post("file", "digest-a")).await?;
    assert_eq!(status, 409, "changed nested kind must conflict: {conflict}");

    // Accepted row: changed digest must conflict; identical body replays.
    let digest_id = remuda_protocol::CommandId::new();
    let original = post("file", "digest-a");
    let (status, _, body) = post_command(&digest_id, original.clone()).await?;
    assert_eq!(status, 200, "{body}");
    let (status, _, conflict) = post_command(&digest_id, post("file", "digest-b")).await?;
    assert_eq!(
        status, 409,
        "changed nested digest must conflict: {conflict}"
    );
    let (status, _, replay) = post_command(&digest_id, original).await?;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(
        serde_json::from_str::<Value>(replay.trim())?["replayed"],
        json!(true)
    );

    // Queued row: host offline, queue, reconnect, then a changed-kind retry.
    // Restart the Node link: abend the accepting one and confirm offline.
    // (The sends counted so far stay on the first link.)
    let before_offline = sends.load(Ordering::Relaxed);
    {
        // A fresh host+socket pair keeps the queued case isolated; enroll
        // tokens are single-use, so mint a second one.
        let host2 = HostId::new();
        let enroll2 = enroll_token(hub.addr, &cookie).await?;
        let (node_token, link) = accepting_node(
            hub.addr,
            &enroll2,
            &host2,
            "r3-nested-off",
            Arc::new(AtomicUsize::new(0)),
        )
        .await?;
        let instance2 = create_print_instance(hub.addr, &cookie, host2.as_id().as_str()).await?;
        link.abort();
        wait_host_online(hub.addr, &cookie, host2.as_id().as_str(), false).await?;
        let queued_id = remuda_protocol::CommandId::new();
        let path2 = format!("/v1/instances/{instance2}/commands");
        let body = json!({
            "commandId": queued_id.as_id().as_str(),
            "operation": "instance.send",
            "payload": post("image", "digest-q")
        })
        .to_string();
        let (status, _, queued) = http(
            hub.addr,
            "POST",
            &path2,
            &[("Cookie", &cookie)],
            Some(&body),
        )
        .await?;
        assert_eq!(status, 200, "{queued}");
        assert_eq!(
            serde_json::from_str::<Value>(queued.trim())?["command"]["forwarded"],
            json!(false)
        );
        let (node, _) = open_fake_node(hub.addr, &node_token, &host2, "r3-nested-back").await?;
        let sends2 = Arc::new(AtomicUsize::new(0));
        let _link = drive_accepting_node(node, sends2.clone());
        wait_host_online(hub.addr, &cookie, host2.as_id().as_str(), true).await?;

        // Changed kind on the queued row conflicts and must not forward.
        let changed = json!({
            "commandId": queued_id.as_id().as_str(),
            "operation": "instance.send",
            "payload": post("file", "digest-q")
        })
        .to_string();
        let (status, _, conflict) = http(
            hub.addr,
            "POST",
            &path2,
            &[("Cookie", &cookie)],
            Some(&changed),
        )
        .await?;
        assert_eq!(
            status, 409,
            "queued nested-kind change must conflict: {conflict}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            sends2.load(Ordering::Relaxed),
            0,
            "the conflict never forwards"
        );

        // The identical retry then forwards exactly once.
        let (status, _, delivered) = http(
            hub.addr,
            "POST",
            &path2,
            &[("Cookie", &cookie)],
            Some(&body),
        )
        .await?;
        assert_eq!(status, 200, "{delivered}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(sends2.load(Ordering::Relaxed), 1);
    }
    assert_eq!(
        sends.load(Ordering::Relaxed),
        before_offline,
        "first link saw no queued-case traffic"
    );
    Ok(())
}

/// The effective top-level anchor index is executable: a retry that moves the
/// attachment from index 1 to index 2 is a 409, while an omitted index
/// (defaulting to position 1) matches the stored manifest.
#[tokio::test]
async fn same_command_id_changed_top_level_anchor_index_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r3-index", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    let (status, _, uploaded) = http(
        hub.addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", &cookie)],
        Some("anchor file"),
    )
    .await?;
    assert_eq!(status, 200, "{uploaded}");
    let object_id = serde_json::from_str::<Value>(uploaded.trim())?["objectId"]
        .as_str()
        .unwrap()
        .to_string();

    let command_id = remuda_protocol::CommandId::new();
    let with_index = |index: Option<u64>| {
        let mut entry = serde_json::Map::new();
        entry.insert("objectId".to_string(), json!(object_id));
        if let Some(index) = index {
            entry.insert("index".to_string(), json!(index));
        }
        json!({
            "commandId": command_id.as_id().as_str(),
            "operation": "instance.send",
            "payload": { "text": "anchored", "attachments": [Value::Object(entry)] }
        })
        .to_string()
    };
    let (status, _, first) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&with_index(Some(1))),
    )
    .await?;
    assert_eq!(status, 200, "{first}");
    let first: Value = serde_json::from_str(first.trim())?;
    assert_eq!(
        first["command"]["payload"]["attachments"][0]["index"],
        json!(1)
    );

    // Move the anchor: conflict.
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&with_index(Some(2))),
    )
    .await?;
    assert_eq!(
        status, 409,
        "changed anchor index must conflict: {conflict}"
    );

    // Omitted index defaults to the 1-based position and matches index 1.
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&with_index(None)),
    )
    .await?;
    assert_eq!(
        status, 200,
        "omitted index defaults to position 1: {replay}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(replay.trim())?["replayed"],
        json!(true)
    );
    // Explicit 1 also replays.
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&with_index(Some(1))),
    )
    .await?;
    assert_eq!(status, 200, "{replay}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        1,
        "only the first POST dispatched"
    );
    Ok(())
}

/// An out-of-i64-range anchor index is cast negative by the first-POST
/// validator and stored as the position fallback; the identical retry must
/// apply the same fallback and replay, not 409.
#[tokio::test]
async fn same_command_id_replay_with_out_of_range_anchor_index_matches() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r4-index-range", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    let (status, _, uploaded) = http(
        hub.addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", &cookie)],
        Some("range file"),
    )
    .await?;
    assert_eq!(status, 200, "{uploaded}");
    let object_id = serde_json::from_str::<Value>(uploaded.trim())?["objectId"]
        .as_str()
        .unwrap()
        .to_string();

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": {
            "text": "huge index",
            "attachments": [{
                "objectId": object_id,
                "index": (i64::MAX as u64) + 1
            }]
        }
    })
    .to_string();
    let (status, _, first) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{first}");
    let first: Value = serde_json::from_str(first.trim())?;
    assert_eq!(
        first["command"]["payload"]["attachments"][0]["index"],
        json!(1),
        "the validator fell back to position 1"
    );
    let (status, _, replay) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(
        status, 200,
        "identical out-of-range retry must replay: {replay}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(replay.trim())?["replayed"],
        json!(true)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sends.load(Ordering::Relaxed), 1);
    Ok(())
}

/// A same-id `instance.configure` replay is read-only: it returns the stored
/// command without re-running the spec merge, so it can never clobber a
/// configuration applied by a later command. A first POST whose merge failed
/// leaves an unmerged stored row; its replay returns that row rather than a
/// false fresh success.
#[tokio::test]
async fn configure_replay_is_read_only_and_keeps_newer_configuration() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r4-configure", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    // Configure A (id X): model opus.
    let id_x = remuda_protocol::CommandId::new();
    let cmd_a = json!({
        "commandId": id_x.as_id().as_str(),
        "operation": "instance.configure",
        "payload": { "model": "opus" }
    })
    .to_string();
    let (status, _, original) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&cmd_a),
    )
    .await?;
    assert_eq!(status, 200, "{original}");
    let original: Value = serde_json::from_str(original.trim())?;
    assert_eq!(original["replayed"], json!(false));

    // Configure B (id Y): model haiku — the newer current configuration.
    let id_y = remuda_protocol::CommandId::new();
    let cmd_b = json!({
        "commandId": id_y.as_id().as_str(),
        "operation": "instance.configure",
        "payload": { "model": "haiku" }
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&cmd_b),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // Replay A: read-only. The stored row X comes back, B's spec survives.
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&cmd_a),
    )
    .await?;
    assert_eq!(status, 200, "{replay}");
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(
        replay["command"], original["command"],
        "the replay's complete command outcome equals X's original response"
    );
    let (status, _, instance) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{instance}");
    assert_eq!(
        serde_json::from_str::<Value>(instance.trim())?["model"],
        json!("haiku"),
        "replaying A must not clobber B's newer configuration"
    );

    // Identifier precedence applies to configure too: replaying X with a key
    // the keyless original never carried is 409, not a read-only success.
    let cmd_a_other_key = json!({
        "commandId": id_x.as_id().as_str(),
        "operation": "instance.configure",
        "payload": { "model": "opus" },
        "idempotencyKey": "reconnb-r4-configure-foreign-key"
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&cmd_a_other_key),
    )
    .await?;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(
        serde_json::from_str::<Value>(conflict.trim())?["code"],
        json!("COMMAND_ID_CONFLICT")
    );

    // A configure whose first-POST merge failed persists that outcome: the
    // replay reproduces the SAME 500 with the same body (it does not re-run
    // the merge and does not become a do-nothing replayed success).
    let bad_id = remuda_protocol::CommandId::new();
    let bad = json!({
        "commandId": bad_id.as_id().as_str(),
        "operation": "instance.configure",
        "payload": { "effort": { "name": 123 } }
    })
    .to_string();
    let (status, _, first) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&bad)).await?;
    assert_eq!(status, 500, "the first merge error must surface: {first}");
    let first: Value = serde_json::from_str(first.trim())?;
    assert_eq!(first["code"], json!("INTERNAL"), "{first}");
    let (status, _, replay) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&bad)).await?;
    assert_eq!(
        status, 500,
        "a replay must reproduce the original merge failure: {replay}"
    );
    let replay: Value = serde_json::from_str(replay.trim())?;
    assert_eq!(replay, first, "the replay body equals the original failure");
    let (status, _, instance) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{instance}");
    let instance: Value = serde_json::from_str(instance.trim())?;
    assert_eq!(
        instance["model"],
        json!("haiku"),
        "the failed configure's replay applies nothing"
    );

    // And the failed row reads back as a rejected, unforwarded settlement.
    let (status, _, row) = http(
        hub.addr,
        "GET",
        &format!("{path}/{}", bad_id.as_id().as_str()),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{row}");
    let row: Value = serde_json::from_str(row.trim())?;
    assert_eq!(row["state"], json!("settled"), "{row}");
    assert_eq!(row["forwarded"], json!(false), "{row}");
    assert_eq!(row["settlement"]["outcome"], json!("rejected"), "{row}");
    Ok(())
}

/// Idempotency-key precedence on the same-id send shortcut: the key is part
/// of the command's identity. A replay may omit the key, but ANY key that
/// differs from the one stored with the commandId — bound elsewhere or merely
/// unused — is 409.
#[tokio::test]
async fn command_id_replay_enforces_idempotency_key_precedence() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "h", "method": "runtime.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    node.close(None).await.ok();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    // Command B stored under key Kb.
    let key_b = "reconnb-r3-key-b";
    let body_b = json!({
        "operation": "instance.send",
        "payload": { "text": "payload q" },
        "idempotencyKey": key_b
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_b),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // Command A with its own id, no key.
    let id_a = remuda_protocol::CommandId::new();
    let payload_a = json!({ "text": "payload p" });
    let body_a = json!({
        "commandId": id_a.as_id().as_str(),
        "operation": "instance.send",
        "payload": payload_a
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_a),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(body.trim())?["command"]["forwarded"],
        json!(false)
    );

    // Retry A carrying B's key: the identifiers disagree -> 409.
    let body_a_key_b = json!({
        "commandId": id_a.as_id().as_str(),
        "operation": "instance.send",
        "payload": payload_a,
        "idempotencyKey": key_b
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_a_key_b),
    )
    .await?;
    assert_eq!(
        status, 409,
        "a key bound to another row must conflict: {conflict}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(conflict.trim())?["code"],
        json!("COMMAND_ID_CONFLICT")
    );

    // Command C stored with key Kc: replay with the same key is fine.
    let id_c = remuda_protocol::CommandId::new();
    let key_c = "reconnb-r3-key-c";
    let body_c = json!({
        "commandId": id_c.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "payload r" },
        "idempotencyKey": key_c
    })
    .to_string();
    let (status, _, first) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_c),
    )
    .await?;
    assert_eq!(status, 200, "{first}");
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_c),
    )
    .await?;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(
        serde_json::from_str::<Value>(replay.trim())?["replayed"],
        json!(true)
    );

    // A replay with a different, otherwise-unused key is still a different
    // request identity -> 409 (even though K2 binds no other command yet).
    let body_c_fresh_key = json!({
        "commandId": id_c.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "payload r" },
        "idempotencyKey": "reconnb-r4-key-unused"
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_c_fresh_key),
    )
    .await?;
    assert_eq!(
        status, 409,
        "a different unused key on a keyed command's replay must conflict: {conflict}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(conflict.trim())?["code"],
        json!("COMMAND_ID_CONFLICT")
    );

    // Replaying the keyed command WITHOUT a key is fine.
    let body_c_no_key = json!({
        "commandId": id_c.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "payload r" }
    })
    .to_string();
    let (status, _, replay) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_c_no_key),
    )
    .await?;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(
        serde_json::from_str::<Value>(replay.trim())?["replayed"],
        json!(true)
    );

    // And supplying a key on a command originally stored WITHOUT one also
    // conflicts — the original had no such identity component.
    let body_a_with_key = json!({
        "commandId": id_a.as_id().as_str(),
        "operation": "instance.send",
        "payload": payload_a,
        "idempotencyKey": "reconnb-r4-key-on-keyless"
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&body_a_with_key),
    )
    .await?;
    assert_eq!(
        status, 409,
        "adding a key to a keyless command must conflict: {conflict}"
    );
    Ok(())
}

/// A never-forwarded queued send whose attachment has since expired is refused
/// with the same 400 a fresh send gets on reconnect, instead of being
/// forwarded to a Node that can no longer pull the object.
#[tokio::test]
async fn queued_unforwarded_send_with_expired_attachment_is_rejected() -> Result<()> {
    let (hub, bootstrap, dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let (node_token, link) = accepting_node(
        hub.addr,
        &enroll,
        &host_id,
        "r3-expired",
        Arc::new(AtomicUsize::new(0)),
    )
    .await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;

    let (status, _, uploaded) = http(
        hub.addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", &cookie)],
        Some("expires while queued"),
    )
    .await?;
    assert_eq!(status, 200, "{uploaded}");
    let object_id = serde_json::from_str::<Value>(uploaded.trim())?["objectId"]
        .as_str()
        .unwrap()
        .to_string();

    // Host goes offline, then the send queues unforwarded.
    link.abort();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;
    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": {
            "text": "deliver with file later",
            "attachments": [{ "objectId": object_id }]
        }
    })
    .to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, _, queued) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{queued}");
    let queued: Value = serde_json::from_str(queued.trim())?;
    assert_eq!(queued["command"]["state"], json!("queued"));
    assert_eq!(queued["command"]["forwarded"], json!(false));

    // The object outlives the host's offline window.
    let db = rusqlite::Connection::open(dir.path().join("data").join("hub.sqlite"))?;
    db.execute(
        "UPDATE objects SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![object_id],
    )?;

    // Host returns; the same-id retry must be refused, not forwarded.
    let (node, _) = open_fake_node(hub.addr, &node_token, &host_id, "r3-expired-back").await?;
    let sends = Arc::new(AtomicUsize::new(0));
    let _link = drive_accepting_node(node, sends.clone());
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), true).await?;
    let (status, _, rejected) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(
        status, 400,
        "an expired attachment on an unsent row must 400: {rejected}"
    );
    let rejected: Value = serde_json::from_str(rejected.trim())?;
    assert_eq!(rejected["code"], json!("BAD_REQUEST"), "{rejected}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sends.load(Ordering::Relaxed),
        0,
        "the Node never received the dead-object send"
    );

    // The row remains queued and unforwarded, so a fresh upload under a NEW
    // commandId is the documented recovery path.
    let (status, _, row) = http(
        hub.addr,
        "GET",
        &format!("{path}/{}", command_id.as_id().as_str()),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{row}");
    let row: Value = serde_json::from_str(row.trim())?;
    assert_eq!(row["forwarded"], json!(false), "{row}");
    assert_eq!(row["state"], json!("queued"), "{row}");
    Ok(())
}

/// Two concurrent FIRST POSTs of one commandId with the same payload but
/// different idempotency keys must serialize to one command plus one 409 —
/// the store's writer job enforces (commandId, key) identity atomically, so
/// the divergent key cannot ride the same-id replay branch.
#[tokio::test]
async fn concurrent_same_id_first_posts_with_different_keys_one_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (_token, _link) =
        accepting_node(hub.addr, &enroll, &host_id, "r5-key-race", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    // Repeat to cover both interleavings (handler-prefilter and the
    // writer-job atomic check); the outcome is required in every round.
    for round in 0..6 {
        let command_id = remuda_protocol::CommandId::new();
        let make = |key: &str| {
            json!({
                "commandId": command_id.as_id().as_str(),
                "operation": "instance.send",
                "payload": { "text": format!("key race {round}") },
                "idempotencyKey": key
            })
            .to_string()
        };
        let body1 = make(&format!("reconnb-r5-k1-{round}"));
        let body2 = make(&format!("reconnb-r5-k2-{round}"));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let post = |body: String| {
            let addr = hub.addr;
            let cookie = cookie.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                http(addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await
            })
        };
        let one = post(body1);
        let two = post(body2);
        let (a, b) = tokio::join!(one, two);
        let results = [a??, b??];
        let statuses: Vec<u16> = results.iter().map(|(status, _, _)| *status).collect();
        assert!(
            statuses.contains(&200) && statuses.contains(&409),
            "round {round}: exactly one POST succeeds, one conflicts, got {statuses:?}"
        );
        for (status, _, body) in &results {
            if *status == 409 {
                let body: Value = serde_json::from_str(body.trim())?;
                assert_eq!(body["code"], json!("COMMAND_ID_CONFLICT"), "{body}");
            }
        }
    }
    // Let any dispatched frame land; exactly one command per round.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, _, listed) = http(
        hub.addr,
        "GET",
        &format!("{path}?limit=50"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    let race_rows = listed["commands"]
        .as_array()
        .context("commands")?
        .iter()
        .filter(|row| {
            row["payload"]["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("key race "))
        })
        .count();
    assert_eq!(
        race_rows, 6,
        "one committed command per round, no duplicate rows"
    );
    assert_eq!(sends.load(Ordering::Relaxed), 6, "one dispatch per round");

    // Deterministic identity check: commit a command under one key, reject the
    // same commandId under a second key, then show the rejected key was never
    // bound and starts a legitimately fresh command on its own POST.
    let bound_id = remuda_protocol::CommandId::new();
    let bound = json!({
        "commandId": bound_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "bound key" },
        "idempotencyKey": "reconnb-r5-bound"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&bound),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let foreign = json!({
        "commandId": bound_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "bound key" },
        "idempotencyKey": "reconnb-r5-unbound-foreign"
    })
    .to_string();
    let (status, _, conflict) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&foreign),
    )
    .await?;
    assert_eq!(status, 409, "{conflict}");
    let foreign_only = json!({
        "operation": "instance.send",
        "payload": { "text": "bound key" },
        "idempotencyKey": "reconnb-r5-unbound-foreign"
    })
    .to_string();
    let (status, _, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&foreign_only),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(body.trim())?["replayed"],
        json!(false),
        "a rejected key was never committed, so its key-only POST is fresh"
    );
    Ok(())
}

/// Concurrent same-id retries against a transport whose forward NEVER queues
/// a frame (`Ok(None)`): every response — followers included, across the
/// attempt-1→attempt-2 handoffs — must report the released queued row
/// (forwarded=false), never a transient forwarded=true.
#[tokio::test]
async fn concurrent_retries_against_dead_forward_all_report_unforwarded() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _, enroll) = device_and_enroll(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let sends = Arc::new(AtomicUsize::new(0));
    let (node_token, link) =
        accepting_node(hub.addr, &enroll, &host_id, "r5-dead", sends.clone()).await?;
    let instance_id = create_print_instance(hub.addr, &cookie, host_id.as_id().as_str()).await?;
    link.abort();
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), false).await?;
    let path = format!("/v1/instances/{instance_id}/commands");

    let command_id = remuda_protocol::CommandId::new();
    let body = json!({
        "commandId": command_id.as_id().as_str(),
        "operation": "instance.send",
        "payload": { "text": "everyone rolls back" }
    })
    .to_string();
    let (status, _, queued) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{queued}");
    assert_eq!(
        serde_json::from_str::<Value>(queued.trim())?["command"]["forwarded"],
        json!(false)
    );

    // Host row live, transport refuses every frame before it queues.
    hub.test_set_node_reply(host_id.as_id().as_str(), None)
        .await;

    // Burst concurrent replays through the leader/follower handoffs.
    let barrier = Arc::new(tokio::sync::Barrier::new(6));
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let body = body.clone();
        let addr = hub.addr;
        let cookie = cookie.clone();
        let path = path.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            http(addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await
        }));
    }
    for task in tasks {
        let (status, _, resp) = task.await??;
        assert_eq!(status, 200, "{resp}");
        let resp: Value = serde_json::from_str(resp.trim())?;
        assert_eq!(resp["replayed"], json!(true));
        assert_eq!(resp["command"]["state"], json!("queued"), "{resp}");
        assert_eq!(
            resp["command"]["forwarded"],
            json!(false),
            "no response may report a forward that never queued: {resp}"
        );
        assert_eq!(resp["command"]["resolution"], json!("clear"), "{resp}");
    }

    // After the real Node returns, one same-id replay delivers it once.
    let (node, _) = open_fake_node(hub.addr, &node_token, &host_id, "r5-dead-back").await?;
    let sends_back = Arc::new(AtomicUsize::new(0));
    let _link = drive_accepting_node(node, sends_back.clone());
    wait_host_online(hub.addr, &cookie, host_id.as_id().as_str(), true).await?;
    let (status, _, delivered) =
        http(hub.addr, "POST", &path, &[("Cookie", &cookie)], Some(&body)).await?;
    assert_eq!(status, 200, "{delivered}");
    assert_eq!(
        serde_json::from_str::<Value>(delivered.trim())?["command"]["forwarded"],
        json!(true)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sends_back.load(Ordering::Relaxed), 1);
    assert_eq!(sends.load(Ordering::Relaxed), 0);
    Ok(())
}
