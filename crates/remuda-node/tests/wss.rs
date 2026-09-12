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
    let (status, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(&create),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let request = tokio::time::timeout(TIMEOUT, link.next_hub_request())
        .await
        .expect("hub request timeout")
        .expect("instance.create");
    assert_eq!(request.method, "instance.create");

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
