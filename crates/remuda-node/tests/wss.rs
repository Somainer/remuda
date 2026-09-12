//! Integration test: outbound WSS carrier against in-process `remuda-hub`.

use remuda_hub::HubConfig;
use remuda_node::{Backoff, WssConfig, WssLink};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(8);

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
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
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, rest.to_string())
}

fn cookie_from(body_and_head: &str) -> Option<String> {
    for line in body_and_head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> (String, String) {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "wss-test"
    })
    .to_string();
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    let req = format!(
        "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let cookie = cookie_from(&text).expect("set-cookie");
    let rest = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let json: Value = serde_json::from_str(rest.trim()).expect("login json");
    let token = json["token"].as_str().expect("device token").to_string();
    (cookie, token)
}

#[tokio::test]
async fn wss_hello_heartbeat_append_reconnect_against_hub() {
    let dir = tempfile::tempdir().expect("tmp");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("data")))
        .await
        .expect("hub");
    let host_id = HostId::new();
    let mut config = WssConfig::loopback(
        hub.addr,
        hub.bootstrap_token.clone(),
        host_id.as_id().as_str().to_owned(),
    );
    config.heartbeat_interval = Duration::from_millis(50);
    config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
    };
    config.journal_queue = 2;

    let mut link = tokio::time::timeout(TIMEOUT, WssLink::connect(config))
        .await
        .expect("connect timeout")
        .expect("wss connect");
    assert!(
        link.hello
            .get("nodeToken")
            .and_then(Value::as_str)
            .is_some(),
        "first hello returns a host token: {}",
        link.hello
    );
    assert_eq!(link.hello["protocol"]["major"], json!(1));

    let instance_id = InstanceId::new();
    let appended = tokio::time::timeout(
        TIMEOUT,
        link.append_journal(
            instance_id.as_id().as_str(),
            json!({
                "schemaVersion": 1,
                "kind": "message",
                "payload": { "role": "assistant", "text": "from-wss-carrier" }
            }),
        ),
    )
    .await
    .expect("append timeout")
    .expect("append");
    assert_eq!(appended["seq"], json!("1"));

    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;
    let (status, hosts) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(status, 200, "{hosts}");
    let hosts: Value = serde_json::from_str(hosts.trim()).expect("hosts json");
    assert_eq!(hosts["items"][0]["online"], json!(true));

    tokio::time::timeout(TIMEOUT, link.reconnect())
        .await
        .expect("reconnect timeout")
        .expect("reconnect");

    let appended = tokio::time::timeout(
        TIMEOUT,
        link.append_journal(
            instance_id.as_id().as_str(),
            json!({ "kind": "message", "payload": { "text": "after-reconnect" } }),
        ),
    )
    .await
    .expect("append2 timeout")
    .expect("append2");
    let seq = appended["seq"].as_str().expect("seq");
    assert!(seq == "2" || seq == "1" || seq.parse::<i64>().unwrap_or(0) >= 1);

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "prompt": "do not replay"
    })
    .to_string();
    let create_task = tokio::spawn({
        let cookie = cookie.clone();
        async move {
            http(
                hub.addr,
                "POST",
                "/v1/instances",
                &[("Cookie", cookie.as_str())],
                Some(&create),
            )
            .await
        }
    });

    let request = tokio::time::timeout(TIMEOUT, link.next_hub_request())
        .await
        .expect("hub request timeout")
        .expect("instance.create");
    assert_eq!(request.method, "instance.create");
    request
        .respond(Ok(json!({ "accepted": true })))
        .await
        .expect("application reply");
    let (status, body) = tokio::time::timeout(TIMEOUT, create_task)
        .await
        .expect("create response timeout")
        .expect("create task");
    assert_eq!(status, 200, "{body}");

    tokio::time::timeout(TIMEOUT, link.reconnect())
        .await
        .expect("reconnect2")
        .expect("reconnect2");

    let replay = tokio::time::timeout(Duration::from_millis(200), link.next_hub_request()).await;
    assert!(
        replay.is_err(),
        "reconnect must not replay Hub commands, got {replay:?}"
    );

    link.shutdown().await;
}

#[tokio::test]
async fn wss_runtime_create_follow_cancel_reconnect_without_duplicates() {
    use futures::StreamExt;
    use remuda_node::{DevNode, DevServerConfig};
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let dir = tempfile::tempdir().expect("tmp");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("data")))
        .await
        .expect("hub");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("dev node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let mut config = WssConfig::loopback(hub.addr, hub.bootstrap_token.clone(), host_id.clone());
    config.heartbeat_interval = Duration::from_millis(80);
    config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
    };

    let mut link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(config, node.clone()))
        .await
        .expect("connect timeout")
        .expect("wss runtime connect");

    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;
    let create = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-print",
        "prompt": "hello-runtime"
    })
    .to_string();
    let (status, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(&create),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(body.trim()).expect("create json");
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();
    let journal_path = format!("/v1/instances/{instance_id}/journal");
    let mut journal_body = String::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let (jstatus, body) = http(
            hub.addr,
            "GET",
            &journal_path,
            &[("Cookie", cookie.as_str())],
            None,
        )
        .await;
        assert_eq!(jstatus, 200, "{body}");
        journal_body = body;
        if journal_body.contains("hello-runtime") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut follow_req = format!("ws://{}/v1/follow?instanceId={instance_id}", hub.addr)
        .into_client_request()
        .expect("follow url");
    follow_req
        .headers_mut()
        .insert("Cookie", cookie.parse().expect("cookie header"));
    let (mut follow, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(follow_req))
            .await
            .expect("follow timeout")
            .expect("follow connect");

    let mut seqs = Vec::new();
    let mut saw_hello = journal_body.contains("hello-runtime");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !saw_hello && tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(WsMessage::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(400), follow.next()).await
        else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).expect("follow json");
        collect_follow_seqs(&frame, &mut seqs);
        if text.contains("hello-runtime") {
            saw_hello = true;
        }
    }
    let local = node
        .get_instance(&instance_id.parse().expect("ins id"))
        .ok()
        .and_then(|inst| node.read_journal(&inst.journal_id, None, 128).ok())
        .map(|page| serde_json::to_string(&page.events).unwrap_or_default())
        .unwrap_or_default();
    assert!(
        saw_hello,
        "expected FakeDriver output on hub follow; journal={journal_body} local={local} seqs={seqs:?}"
    );
    assert_unique_seqs(&seqs);

    let cancel = json!({ "operation": "instance.cancel", "payload": {} }).to_string();
    let path = format!("/v1/instances/{instance_id}/commands");
    let (status, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", cookie.as_str())],
        Some(&cancel),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let mut saw_cancel = false;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(WsMessage::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(400), follow.next()).await
        else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).expect("follow json");
        collect_follow_seqs(&frame, &mut seqs);
        if text.contains("cancelled") {
            saw_cancel = true;
            break;
        }
    }
    assert!(saw_cancel, "expected cancel lifecycle on hub follow");
    assert_unique_seqs(&seqs);

    tokio::time::timeout(TIMEOUT, link.reconnect())
        .await
        .expect("reconnect timeout")
        .expect("reconnect");

    let replay = tokio::time::timeout(Duration::from_millis(250), link.next_hub_request()).await;
    assert!(
        replay.is_err(),
        "reconnect must not replay Hub commands, got {replay:?}"
    );

    let send = json!({
        "operation": "instance.send",
        "payload": { "prompt": "second-turn" }
    })
    .to_string();
    let (status, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", cookie.as_str())],
        Some(&send),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let mut saw_second = false;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(WsMessage::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(400), follow.next()).await
        else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).expect("follow json");
        collect_follow_seqs(&frame, &mut seqs);
        if text.contains("second-turn") {
            saw_second = true;
            break;
        }
    }
    assert!(
        saw_second,
        "expected second FakeDriver turn after reconnect"
    );
    assert_unique_seqs(&seqs);

    link.shutdown().await;
}

fn collect_follow_seqs(frame: &Value, seqs: &mut Vec<String>) {
    if frame.get("type").and_then(Value::as_str) == Some("event")
        && let Some(seq) = frame.get("seq")
    {
        seqs.push(seq.to_string());
    }
}

fn assert_unique_seqs(seqs: &[String]) {
    let mut seen = std::collections::BTreeSet::new();
    for seq in seqs {
        assert!(seen.insert(seq.clone()), "duplicate hub follow seq {seq}");
    }
}
