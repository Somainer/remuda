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
    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", hub.bootstrap_token).parse()?,
    );
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

#[tokio::test]
async fn healthz_ok() -> Result<()> {
    let (hub, _, _dir) = boot().await?;
    let (status, _, body) = http(hub.addr, "GET", "/healthz", &[], None).await?;
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true") || body.contains("\"ok\": true"));
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
    let (cookie, _) = login(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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
    let (cookie, _) = login(hub.addr, &bootstrap).await?;

    // Enroll a host then drop the socket so it is offline.
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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
    tokio::time::sleep(Duration::from_millis(100)).await;

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
    let (cookie, _) = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let (node, _) = {
        let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {bootstrap}").parse().unwrap(),
        );
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
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
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
async fn get_instance_and_follow_with_query_token() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (_cookie, token) = login(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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

    let mut follow_req = format!(
        "ws://{}/v1/follow?instanceId={}&token={}",
        hub.addr, instance_id, token
    )
    .into_client_request()?;
    follow_req.headers_mut().remove("Authorization");
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req))
            .await
            .context("follow query token")??;
    let snapshot = recv_json(&mut follow).await?;
    assert_eq!(snapshot["type"], json!("snapshot"));
    assert_eq!(snapshot["instanceId"], json!(instance_id));
    Ok(())
}

#[tokio::test]
async fn create_instance_persists_delegation_and_provider_profile() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let (cookie, _) = login(hub.addr, &bootstrap).await?;

    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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
        "delegation": "gateway",
        "settingsOverlayPath": "~/.claude/settings.relay.json",
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
    assert_eq!(body["instance"]["delegation"], json!("gateway"));
    assert_eq!(body["instance"]["providerProfileId"], json!("gateway"));
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
    assert_eq!(got["delegation"], json!("gateway"));
    assert_eq!(got["providerProfileId"], json!("gateway"));

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
    assert_eq!(item["delegation"], json!("gateway"));
    assert_eq!(item["providerProfileId"], json!("gateway"));
    Ok(())
}

#[tokio::test]
async fn second_hello_on_socket_is_rejected() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
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
    let mut req = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("node request")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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
    let (cookie, _) = login(hub.addr, &bootstrap).await?;
    let host_id = HostId::new();
    let mut req_a = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .context("a")?;
    req_a.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
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
    tokio::time::sleep(Duration::from_millis(80)).await;

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
    assert_eq!(hosts["items"][0]["online"], json!(true), "{hosts}");

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
    let (cookie, _) = login(hub.addr, &bootstrap).await?;
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
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {bootstrap}").parse().unwrap(),
    );
    let (mut node, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .context("connect first")??;
    node.send(Message::Text(hello("1").into())).await?;
    let first = recv_json(&mut node).await?;
    let node_token = first["result"]["nodeToken"]
        .as_str()
        .context("host token")?;

    let mut attacker_req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    attacker_req
        .headers_mut()
        .insert("Authorization", format!("Bearer {bootstrap}").parse()?);
    let (mut attacker, _) = tokio_tungstenite::connect_async(attacker_req).await?;
    attacker
        .send(Message::Text(hello("attacker").into()))
        .await?;
    let rejected = recv_json(&mut attacker).await?;
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert!(rejected.get("result").is_none(), "{rejected}");
    // The failed collision cannot steal or disconnect the victim's route.
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"heartbeat", "method":"runtime.heartbeat", "params":{}})
            .to_string()
            .into(),
    ))
    .await?;
    assert!(recv_json(&mut node).await?.get("result").is_some());
    drop(attacker);

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
