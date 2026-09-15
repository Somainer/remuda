//! Hub derives instance lifecycle from replayed Node journal observations.

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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "lifecycle-phone"
    })
    .to_string();
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

async fn append(
    node: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    rpc_id: &str,
    instance_id: &str,
    event: Value,
) -> Result<()> {
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "journal.append",
            "params": { "instanceId": instance_id, "event": event }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let ack = recv_json(node).await?;
    anyhow::ensure!(ack.get("result").is_some(), "{ack}");
    Ok(())
}

async fn get_instance(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
) -> Result<Value> {
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", cookie)],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "get instance {status} {body}");
    Ok(serde_json::from_str(body.trim())?)
}

/// A create whose RPC reply is lost past the Hub accept deadline still
/// converges: the HTTP call returns `queued / reconciling` (never resent), and
/// the Node's independently-mirrored journal — the durable accept and the
/// materialization phases — wins the row over to `accepted` and the instance
/// to `running`. This is the native-pty-2c failure: on a loaded host the
/// Node's ack landed after the 5 s deadline and the instance stayed
/// `requested / unknown` for the whole 60 s window.
#[tokio::test]
async fn a_late_create_ack_converges_from_the_journal_without_a_resend() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let mut config = HubConfig::for_test(data_dir.clone());
    // Fail the RPC accept fast, but never expire the `requested` row under us.
    config.command_accept_timeout_ms = 50;
    config.requested_grace_ms = 600_000;
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let enroll = enroll_token(addr, &cookie).await?;

    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    let body = json!({
        "kind": "terminal",
        "driver": "shell-pty",
        "hostId": host_id.as_id().as_str()
    })
    .to_string();
    let cookie_clone = cookie.clone();
    // The HTTP call blocks until the 50 ms accept deadline; run it while the
    // fake Node drains the forwarded RPC without ever replying.
    let post = tokio::spawn(async move {
        http(
            addr,
            "POST",
            "/v1/instances",
            &[("Cookie", cookie_clone.as_str())],
            Some(&body),
        )
        .await
    });
    let forwarded = recv_json(&mut node).await?;
    let rpc_id = forwarded["id"]
        .as_str()
        .expect("the forwarded create has an rpc id")
        .to_string();
    let (status, _, body) = tokio::time::timeout(TIMEOUT, post).await??.unwrap();
    assert_eq!(status, 200, "a timeout is still a 200, not an error");
    let created: Value = serde_json::from_str(body.trim())?;
    assert_eq!(created["command"]["state"], json!("queued"));
    assert_eq!(
        created["command"]["resolution"],
        json!("reconciling"),
        "a timed-out accept is unknown → reconciling, not silently dropped"
    );
    assert_eq!(created["instance"]["lifecycle"], json!("requested"));
    let ins = created["instance"]["instanceId"]
        .as_str()
        .or_else(|| created["instanceId"].as_str())
        .expect("instance id")
        .to_owned();
    let command_id = created["command"]["commandId"]
        .as_str()
        .expect("command id")
        .to_owned();

    // The Node durably accepted and materialized; it journals those facts
    // independently of the RPC reply it never got to send.
    append(
        &mut node,
        "s1",
        &ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity",
                "entityType": "command",
                "entityId": command_id,
                "state": "accepted",
                "entity": { "commandId": command_id, "state": "accepted" }
            }
        }),
    )
    .await?;
    append(
        &mut node,
        "s2",
        &ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "starting", "reasonCode": "driver-spawn" }
        }),
    )
    .await?;
    append(
        &mut node,
        "s3",
        &ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "ready", "reasonCode": "driver-started" }
        }),
    )
    .await?;

    // No resend happened: the create rpc id was never answered and the Hub did
    // not forward it again. A second inbound frame would be a journal-related
    // response only; assert there is nothing new queued on the socket.
    let dangling = tokio::time::timeout(Duration::from_millis(150), recv_json(&mut node)).await;
    assert!(
        dangling.is_err(),
        "a lost accept must not be resent: {dangling:?}"
    );
    // Keep the dangling rpc id referenced: this is exactly the unanswered call.
    assert!(!rpc_id.is_empty());

    let view = tokio::time::timeout(TIMEOUT, async {
        loop {
            let view = get_instance(addr, &cookie, &ins).await?;
            if view["lifecycle"] == json!("running") {
                return Ok::<_, anyhow::Error>(view);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    assert_eq!(view["lifecycle"], json!("running"));
    Ok(())
}

#[tokio::test]
async fn journal_replay_derives_lifecycle_and_herdr_idle() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let config = HubConfig::for_test(data_dir.clone());
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let enroll = enroll_token(addr, &cookie).await?;

    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    let host_id = HostId::new();
    let instance_id = InstanceId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    let ins = instance_id.as_id().as_str();
    append(
        &mut node,
        "s1",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "starting", "reasonCode": "driver-start" }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("starting"));
    assert_eq!(
        view["activity"],
        json!("unknown"),
        "create/start default must not look idle"
    );

    append(
        &mut node,
        "s2",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "ready", "reasonCode": "driver-started" }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("running"));
    assert_eq!(
        view["activity"],
        json!("unknown"),
        "ready/running is not herdr idle proof"
    );

    let (status, _, listed) = http(
        addr,
        "GET",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    assert_eq!(listed["items"][0]["lifecycle"], json!("running"));
    assert_eq!(listed["items"][0]["activity"], json!("unknown"));

    append(
        &mut node,
        "s3",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "working" }
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("running"));
    assert_eq!(view["activity"], json!("working"));

    append(
        &mut node,
        "s4",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "blocked" }
            }
        }),
    )
    .await?;
    assert_eq!(
        get_instance(addr, &cookie, ins).await?["activity"],
        json!("blocked")
    );

    append(
        &mut node,
        "s5",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "idle" }
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["lifecycle"], json!("running"));
    assert_eq!(view["activity"], json!("idle"));

    append(
        &mut node,
        "s6",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "exited", "reasonCode": "explicit-close" }
        }),
    )
    .await?;
    assert_eq!(
        get_instance(addr, &cookie, ins).await?["lifecycle"],
        json!("exited")
    );

    let failed_id = InstanceId::new();
    append(
        &mut node,
        "f1",
        failed_id.as_id().as_str(),
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity",
                "state": "failed",
                "reasonCode": "native-driver-start-failed"
            }
        }),
    )
    .await?;
    assert_eq!(
        get_instance(addr, &cookie, failed_id.as_id().as_str()).await?["lifecycle"],
        json!("failed")
    );

    drop(node);
    hub.shutdown().await;
    let mut config = HubConfig::for_test(data_dir);
    config.bootstrap_token = bootstrap;
    let hub = spawn(config).await?;
    let view = get_instance(hub.addr, &cookie, ins).await?;
    assert_eq!(
        view["lifecycle"],
        json!("exited"),
        "lifecycle must survive Hub restart"
    );
    assert_eq!(view["activity"], json!("idle"));
    Ok(())
}

#[tokio::test]
async fn pty_create_stays_accepted_with_a_queued_prompt_and_native_dialog() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;
    let enroll = enroll_token(addr, &cookie).await?;
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "node.hello",
            "params": {
                "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0",
                "host": { "herdr": { "version": "0.9.0" } }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    assert!(recv_json(&mut node).await?.get("result").is_some());

    let create_cookie = cookie.clone();
    let create = tokio::spawn(async move {
        http(
            addr,
            "POST",
            "/v1/instances",
            &[("Cookie", &create_cookie)],
            Some(
                &json!({
                    "hostId": host_id.as_id().as_str(), "kind": "claude",
                    "driver": "claude-pty", "delegation": "none",
                    "permissionMode": "bypass", "prompt": "Reply with exactly PONG."
                })
                .to_string(),
            ),
        )
        .await
    });
    let request = recv_json(&mut node).await.context("forwarded PTY create")?;
    assert_eq!(request["method"], "instance.create");
    let instance_id = request["params"]["instanceId"]
        .as_str()
        .context("instanceId")?;
    let command_id = request["params"]["commandId"]
        .as_str()
        .context("commandId")?;
    // The fake Node has launched the PTY, but control is blocked on its trust
    // dialog. Resource creation settles successfully while native input waits.
    let interaction_id = remuda_protocol::InteractionId::new();
    for (rpc_id, event) in [
        (
            "ready",
            json!({ "kind": "lifecycle", "payload": {
                "type": "entity", "state": "ready", "reasonCode": "driver-started"
            }}),
        ),
        (
            "queued",
            json!({ "kind": "message", "payload": {
                "operation": "open", "nodeId": "nod_queued", "revision": "1",
                "baseRevision": null,
                "messageId": "msg_queued", "role": "user", "status": "queued",
                "blocks": [{ "type": "text", "text": "Reply with exactly PONG." }]
            }}),
        ),
        (
            "dialog",
            json!({ "kind": "interaction.requested", "payload": { "interaction": {
                "id": interaction_id.as_id().as_str(), "kind": "question",
                "state": "pending", "blocking": true, "carrier": "native-tty",
                "prompt": "Quick safety check: Is this a project you created or one you trust?"
            }}}),
        ),
        (
            "settled",
            json!({ "kind": "lifecycle", "payload": {
                "type": "entity", "entityType": "command", "state": "settled",
                "entity": { "commandId": command_id, "operation": "instance.create",
                    "state": "settled", "settlement": { "state": "known", "value": {
                        "outcome": "completed", "error": null
                    }}
                }
            }}),
        ),
    ] {
        append(&mut node, rpc_id, instance_id, event)
            .await
            .with_context(|| format!("append {rpc_id}"))?;
    }

    // Mirroring may outrun the RPC receipt. The Hub must preserve the successful
    // settlement even though the queued prompt and native dialog remain pending.
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": request["id"],
            "result": { "command": { "commandId": command_id, "state": "accepted" } }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let (status, _, body) = tokio::time::timeout(TIMEOUT, create).await???;
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(body.trim())?;
    assert_eq!(created["command"]["state"], "settled");
    assert_eq!(created["command"]["resolution"], "clear");

    let view = get_instance(addr, &cookie, instance_id).await?;
    assert_eq!(view["lifecycle"], "running", "{view}");
    assert_eq!(view["activity"], "blocked", "{view}");
    assert!(view["lastError"].is_null(), "{view}");
    let (status, _, body) = http(
        addr,
        "GET",
        "/v1/interactions",
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let pending: Value = serde_json::from_str(body.trim())?;
    assert_eq!(pending["items"][0]["state"], "pending");
    assert_eq!(pending["items"][0]["kind"], "question");
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}/journal"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let journal: Value = serde_json::from_str(body.trim())?;
    let events = journal["events"].as_array().context("journal events")?;
    assert_eq!(events[1]["event"]["payload"]["status"], "queued");
    assert_eq!(
        events[3]["event"]["payload"]["entity"]["settlement"]["value"]["outcome"],
        "completed"
    );
    assert_eq!(
        events[3]["event"]["payload"]["entity"]["commandId"],
        command_id
    );
    node.close(None).await?;
    hub.shutdown().await;
    Ok(())
}

/// D-025: a terminal instance whose PTY foreground becomes `claude` must show
/// up as a promoted Claude session on the Hub, and fall back on demotion. The
/// driver never changes — only `kind`, `mode` and `promotedAt`.
#[tokio::test]
async fn journal_replay_promotes_and_demotes_a_terminal_instance() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let enroll = enroll_token(addr, &cookie).await?;

    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": { "hostId": host_id.as_id().as_str(), "nodeVersion": "0.1.0" }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = recv_json(&mut node).await?;

    let instance_id = InstanceId::new();
    let ins = instance_id.as_id().as_str();
    append(
        &mut node,
        "p0",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "ready", "reasonCode": "driver-started" }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert!(
        view["mode"].is_null() || view["mode"] == json!("native"),
        "an untouched instance is not promoted: {view}"
    );

    // The human typed `claude`; the driver journals the detection, then the
    // promotion. Only the second one moves the entity.
    append(
        &mut node,
        "p1",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_detected",
                "status": { "state": "known", "value": "agent detected: claude" },
                "relatedIds": { "kind": "claude", "pid": "4242" },
                "severity": "info"
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(
        view["kind"], "claude",
        "the create-time kind is claude in this fixture: {view}"
    );
    assert!(
        view["mode"].is_null() || view["mode"] == json!("native"),
        "the diagnostic alone must not promote: {view}"
    );

    append(
        &mut node,
        "p2",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_promoted",
                "status": { "state": "known", "value": "claude" },
                "relatedIds": {
                    "kind": "claude",
                    "mode": "promoted",
                    "promotedAt": "2026-09-13T10:00:00.000Z",
                    "sessionId": "11111111-2222-4333-8444-555555555555"
                },
                "severity": "info"
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["kind"], "claude", "{view}");
    assert_eq!(view["mode"], "promoted", "{view}");
    assert_eq!(view["promotedAt"], "2026-09-13T10:00:00.000Z", "{view}");
    assert_eq!(
        view["lifecycle"], "running",
        "promotion is not a lifecycle change: {view}"
    );

    // The list view the session picker reads must agree with the detail.
    let (status, _, listed) = http(
        addr,
        "GET",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim())?;
    assert_eq!(listed["items"][0]["kind"], "claude", "{listed}");
    assert_eq!(listed["items"][0]["mode"], "promoted", "{listed}");

    // `/exit` in the terminal: back to a plain shell, promotedAt cleared.
    append(
        &mut node,
        "p3",
        ins,
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_demoted",
                "status": { "state": "known", "value": "terminal" },
                "relatedIds": {
                    "kind": "terminal",
                    "mode": "native",
                    "previousKind": "claude"
                },
                "severity": "info"
            }
        }),
    )
    .await?;
    let view = get_instance(addr, &cookie, ins).await?;
    assert_eq!(view["kind"], "terminal", "{view}");
    assert_eq!(view["mode"], "native", "{view}");
    assert!(view["promotedAt"].is_null(), "{view}");
    assert_eq!(
        view["lifecycle"], "running",
        "demotion is not a close: {view}"
    );

    node.close(None).await?;
    hub.shutdown().await;
    Ok(())
}
