//! Cross-host agent-to-agent messaging (host-search-1, D-031 follow-up).
//!
//! An Agent instance on node A may `instance.send` to an instance on node B
//! with no per-message human approval when B lies inside the caller's project
//! scope. The send is journaled on both sides (outbound by the sender, inbound
//! by the receiver). An out-of-scope target keeps the one-shot human
//! Interaction (409 HUMAN_APPROVAL_REQUIRED because the request carries no
//! the fleet `all` broadcast stays banned from Agent origin.

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

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(text) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", text.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(text) = body {
        request.extend_from_slice(text.as_bytes());
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("no header terminator"))?;
    let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Ok((
        status,
        String::from_utf8_lossy(&buf[split + 4..]).to_string(),
    ))
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
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

/// Enroll one fake Node, answer every Hub RPC with `{accepted: true}`, and
/// return its `hst_…` id. The answering task keeps the socket alive for the
/// whole test.
async fn enroll_node(addr: std::net::SocketAddr, cookie: &str, label: &str) -> Result<String> {
    let (_, token_body) = http(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", cookie)],
        Some("{}"),
    )
    .await?;
    let enroll = serde_json::from_str::<Value>(token_body.trim())?["token"]
        .as_str()
        .context("enroll token")?
        .to_owned();

    let host_id = HostId::new().as_id().as_str().to_owned();
    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut ws, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "node.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": label,
                "host": {
                    "labels": {},
                    "maxInstances": 8,
                    "cli": [{ "kind": "claude", "version": "2.1.268",
                              "path": "/usr/bin/claude", "auth": "unknown" }],
                    "resources": { "cpuPct": 1, "memPct": 2 }
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut ws).await?;
    anyhow::ensure!(hello["result"]["hostId"] == json!(host_id), "{hello}");

    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            // Notifications carry no id and need no answer.
            let Some(id) = frame.get("id").filter(|id| !id.is_null()) else {
                continue;
            };
            let _ = ws
                .send(Message::Text(
                    json!({"jsonrpc":"2.0","id":id,"result":{"accepted":true}})
                        .to_string()
                        .into(),
                ))
                .await;
        }
    });
    Ok(host_id)
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({"bootstrapToken": bootstrap, "deviceName": "cross-host-phone"}).to_string();
    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!(
                "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("header terminator")?;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    anyhow::ensure!(head.contains(" 200 "), "{head}");
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            return Ok(line
                .split_once(':')
                .unwrap()
                .1
                .trim()
                .split(';')
                .next()
                .unwrap()
                .trim()
                .to_owned());
        }
    }
    Err(anyhow!("login set-cookie missing"))
}

/// Create a claude-print instance (no live model) pinned to one host.
async fn create_instance(
    addr: std::net::SocketAddr,
    cookie: &str,
    host: &str,
    scope: Value,
    title: &str,
) -> Result<String> {
    let body = json!({
        "hostId": host,
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "scope": scope,
        "title": title
    })
    .to_string();
    let (status, reply) = http(
        addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie)],
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "create {title} on {host}: {status} {reply}");
    let created = serde_json::from_str::<Value>(reply.trim())?;
    created
        .pointer("/instance/instanceId")
        .and_then(Value::as_str)
        .context("instanceId in create response")
        .map(str::to_owned)
}

fn send_body(text: &str) -> String {
    json!({
        "operation": "instance.send",
        "payload": {
            "input": {
                "type": "prompt",
                "mode": "new-turn",
                "blocks": [{ "type": "text", "text": text }],
                "origin": "agent"
            },
            "completionScope": "native-turn"
        }
    })
    .to_string()
}

#[tokio::test]
async fn agent_messages_remote_instance_within_scope_and_journals_both_sides() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await?;

    let host_a = enroll_node(addr, &cookie, "cross-host-a").await?;
    let host_b = enroll_node(addr, &cookie, "cross-host-b").await?;

    // Sender A1 is scoped to both hosts; A2 only to its own host.
    let a1 = create_instance(
        addr,
        &cookie,
        &host_a,
        json!({ "hostIds": [host_a.clone(), host_b.clone()] }),
        "sender-wide",
    )
    .await?;
    let a2 = create_instance(
        addr,
        &cookie,
        &host_a,
        json!({ "hostIds": [host_a.clone()] }),
        "sender-narrow",
    )
    .await?;
    let target_b = create_instance(addr, &cookie, &host_b, json!({}), "receiver-b").await?;

    let token_wide = hub.test_mint_agent_token("agent-a1", &a1).await?;
    let token_narrow = hub.test_mint_agent_token("agent-a2", &a2).await?;

    // In-scope cross-host send goes straight through (no human Interaction).
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/instances/{target_b}/commands"),
        &[("Authorization", &format!("Bearer {token_wide}"))],
        Some(&send_body("hello from node A")),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // Out-of-scope target on host B keeps the one-shot human Interaction: the
    // Hub answers 409 with a pending interaction id (D-017).
    let (status, body) = http(
        addr,
        "POST",
        &format!("/v1/instances/{target_b}/commands"),
        &[("Authorization", &format!("Bearer {token_narrow}"))],
        Some(&send_body("this must be refused")),
    )
    .await?;
    assert_eq!(status, 409, "out-of-scope send {body}");
    assert!(body.contains("HUMAN_APPROVAL_REQUIRED"), "{body}");

    // Fleet --all stays banned from Agent origin.
    let (status, _) = http(
        addr,
        "POST",
        "/v1/fleet/broadcast",
        &[("Authorization", &format!("Bearer {token_wide}"))],
        Some(&json!({ "all": true, "text": "x" }).to_string()),
    )
    .await?;
    assert_eq!(status, 400);

    // Sender side: the outbound record names target instance and host.
    let (status, journal) = http(
        addr,
        "GET",
        &format!("/v1/instances/{a1}/journal"),
        &[("Authorization", &format!("Bearer {token_wide}"))],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{journal}");
    let outbound = serde_json::from_str::<Value>(journal.trim())?["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event.pointer("/event/payload/direction") == Some(&json!("outbound")))
        .cloned()
        .context("outbound journal event")?;
    assert_eq!(
        outbound.pointer("/event/payload/relatedIds/instanceId"),
        Some(&json!(target_b))
    );
    assert_eq!(
        outbound.pointer("/event/payload/relatedIds/hostId"),
        Some(&json!(host_b))
    );
    assert_eq!(
        outbound.pointer("/event/payload/text"),
        Some(&json!("hello from node A"))
    );

    // Receiver side: the inbound record names the sender and its host.
    let (status, journal) = http(
        addr,
        "GET",
        &format!("/v1/instances/{target_b}/journal"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{journal}");
    let inbound = serde_json::from_str::<Value>(journal.trim())?["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event.pointer("/event/payload/direction") == Some(&json!("inbound")))
        .cloned()
        .context("inbound journal event")?;
    assert_eq!(
        inbound.pointer("/event/payload/relatedIds/instanceId"),
        Some(&json!(a1))
    );
    assert_eq!(
        inbound.pointer("/event/payload/relatedIds/hostId"),
        Some(&json!(host_a))
    );

    // The refused send wrote nothing on either side.
    let narrow = serde_json::from_str::<Value>(journal.trim())?;
    let events = narrow["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.pointer("/event/payload/text")
                != Some(&json!("this must be refused")))
    );

    hub.shutdown().await;
    Ok(())
}
