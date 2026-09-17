//! Integration test: outbound WSS carrier against in-process `remuda-hub`.

use remuda_hub::HubConfig;
use remuda_node::{Backoff, WssConfig, WssLink, apply_hello_result, load_or_create_enrollment};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(8);
const SLOW_START: Duration = Duration::from_secs(10);
const SLOW_TIMEOUT: Duration = Duration::from_secs(25);

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

/// D-018: a Node enrolls with a single-use enroll token, never the device
/// pairing access code. The in-process Hub mints one directly.
async fn enroll_token(hub: &remuda_hub::RunningHub) -> String {
    hub.mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .expect("mint enroll token")
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
        enroll_token(&hub).await,
        host_id.as_id().as_str().to_owned(),
    );
    config.heartbeat_interval = Duration::from_millis(50);
    config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        jitter_ppt: 0,
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
        "delegation": "none",
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
async fn wss_reannounce_keeps_a_single_host_row() {
    let dir = tempfile::tempdir().expect("tmp");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("data")))
        .await
        .expect("hub");
    let host_id = HostId::new();
    let mut config = WssConfig::loopback(
        hub.addr,
        enroll_token(&hub).await,
        host_id.as_id().as_str().to_owned(),
    );
    config.cli = json!([
        { "kind": "claude", "version": "2.1.268", "path": "/usr/bin/claude", "auth": "unknown" },
        { "kind": "codex", "version": "0.1.0", "path": "/usr/bin/codex", "auth": "unknown" },
        { "kind": "grok", "version": "1.0.0", "path": "/usr/bin/grok", "auth": "unknown" },
        { "kind": "agy", "version": "1.2.1", "path": "/usr/bin/agy", "auth": "unknown" }
    ]);
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect(config.clone()))
        .await
        .expect("connect timeout")
        .expect("first enroll");
    let token = link
        .node_token
        .clone()
        .expect("first hello returns a host token");
    tokio::time::timeout(TIMEOUT, link.shutdown())
        .await
        .expect("shutdown timeout");

    config.token = token;
    config.url = format!("ws://{}/node/v1/connect", hub.addr);
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect(config))
        .await
        .expect("reconnect timeout")
        .expect("reannounce");

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
    let items = hosts["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{hosts}");
    assert_eq!(items[0]["hostId"], json!(host_id.as_id().as_str()));
    assert_eq!(items[0]["online"], json!(true));
    let empty = Vec::new();
    let kinds: Vec<&str> = items[0]["cli"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|row| row["kind"].as_str())
        .collect();
    assert_eq!(kinds, ["claude", "codex", "grok", "agy"], "{items:?}");

    tokio::time::timeout(TIMEOUT, link.shutdown())
        .await
        .expect("second shutdown");
}

#[tokio::test]
async fn enrollment_file_keeps_the_same_host_id_across_hello() {
    let dir = tempfile::tempdir().expect("tmp");
    let identity = dir.path().join("identity");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .expect("hub");
    let first = load_or_create_enrollment(&identity).expect("first identity");
    let mut config = WssConfig::loopback(
        hub.addr,
        enroll_token(&hub).await,
        first.host_id.as_id().as_str().to_owned(),
    );
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect(config.clone()))
        .await
        .expect("connect timeout")
        .expect("first hello");
    apply_hello_result(
        &identity,
        &json!({
            "hostId": first.host_id,
            "nodeToken": link.node_token,
        }),
    )
    .expect("persist");
    tokio::time::timeout(TIMEOUT, link.shutdown())
        .await
        .expect("shutdown");

    let bootstrap = hub.bootstrap_token.clone();
    hub.shutdown().await;
    let hub = remuda_hub::spawn({
        let mut config = HubConfig::for_test(dir.path().join("hub"));
        config.bootstrap_token = bootstrap.clone();
        config
    })
    .await
    .expect("hub restart");

    let second = load_or_create_enrollment(&identity).expect("reload identity");
    assert_eq!(second.host_id, first.host_id);
    // D-018: re-announce presents the host's own stored node token.
    let token = second.node_token.clone().expect("persisted node token");
    config.token = token;
    config.url = format!("ws://{}/v1/node", hub.addr);
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect(config))
        .await
        .expect("reconnect timeout")
        .expect("second hello");
    assert_eq!(link.host_id, first.host_id.as_id().as_str());

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
    let items = hosts["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{hosts}");
    assert_eq!(items[0]["hostId"], json!(first.host_id.as_id().as_str()));
    assert_eq!(items[0]["online"], json!(true));

    tokio::time::timeout(TIMEOUT, link.shutdown())
        .await
        .expect("second shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_wss_doctor_returns_workspace_access_report_through_hub() {
    use remuda_node::{DaemonControl, DevNode, DevServerConfig};

    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let hub = remuda_hub::spawn(HubConfig::for_test(fixture.path().join("hub")))
        .await
        .unwrap();
    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(workspace.clone())
            .with_workspace_roots(vec![fixture.path().to_path_buf()]),
    )
    .unwrap();
    let host_id = node.host().meta.id.as_id().to_string();
    let control = DaemonControl::new().unwrap();
    let lease = control.acquire_outbound().await.unwrap();
    let link = WssLink::connect_runtime_controlled(
        WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id.clone()),
        node,
        lease.clone(),
    )
    .await
    .unwrap();
    // The already registered workspace becomes unavailable while its Node stays online.
    std::fs::remove_dir(&workspace).unwrap();
    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;
    let (status, body) = tokio::time::timeout(
        SLOW_TIMEOUT,
        http(
            hub.addr,
            "GET",
            &format!("/v1/hosts/{host_id}/doctor"),
            &[("Cookie", cookie.as_str())],
            None,
        ),
    )
    .await
    .expect("doctor response deadline");
    assert_eq!(status, 200, "{body}");
    let report: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["exitCode"], 1, "{report}");
    let checks = report["checks"].as_array().expect("actual doctor checks");
    let access = checks
        .iter()
        .find(|check| check["name"] == "workspace.access")
        .expect("workspace access check");
    assert_eq!(access["status"], "blocker", "{access}");
    assert_eq!(access["details"]["path"], workspace.display().to_string());
    assert!(access["message"].as_str().unwrap().contains("inaccessible"));
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
    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let mut config = WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id.clone());
    config.heartbeat_interval = Duration::from_millis(80);
    config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        jitter_ppt: 0,
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
        "model": "haiku",
        "args": ["--max-budget-usd", "0.3"],
        "providerProfileId": "native-login",
        "permissionMode": "dontAsk",
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
    assert_eq!(
        created["command"]["payload"]["spec"]["model"],
        json!("haiku")
    );
    assert_eq!(
        created["command"]["payload"]["spec"]["args"],
        json!(["--max-budget-usd", "0.3"])
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wss_create_is_accepted_before_ten_second_fake_herdr_start() {
    use remuda_node::{
        DevNode, DevServerConfig, MemoryStore, NativeDriverConfig, native_driver_registry,
    };
    use remuda_testing::{FakeHerdrOptions, FakeHerdrServer, ensure_workspace_bin};
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tmp");
    let mut hub_config = HubConfig::for_test(dir.path().join("hub"));
    hub_config.command_accept_timeout_ms = 2_000;
    let hub = remuda_hub::spawn(hub_config).await.expect("hub");

    let socket_dir = dir.path().join("herdr");
    std::fs::create_dir_all(&socket_dir).expect("herdr dir");
    let fake_herdr = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(socket_dir.join("herdr.sock")).with_agent_start_delay(SLOW_START),
    )
    .expect("fake herdr");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut node_config = DevServerConfig::loopback(0);
    node_config.workspace_root = workspace;
    node_config.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let mut native = NativeDriverConfig::new(dir.path().join("node"))
        .with_claude_binary(ensure_workspace_bin("fake-claude"));
    native.herdr_binary = Some(ensure_workspace_bin("fake-herdr"));
    native.herdr_socket_dir = Some(socket_dir);
    let drivers = native_driver_registry(native).expect("native drivers");
    let node =
        DevNode::with_parts(&node_config, Arc::new(MemoryStore::new(256)), drivers).expect("node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let mut config = WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id.clone());
    config.heartbeat_interval = Duration::from_millis(100);
    config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        jitter_ppt: 0,
    };
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(config, node))
        .await
        .expect("connect timeout")
        .expect("wss runtime connect");
    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;

    let request = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "generic-pty",
        "model": "fake",
        "providerProfileId": "native",
        "permissionMode": "dontAsk",
        "prompt": "slow-start"
    })
    .to_string();
    let started = tokio::time::Instant::now();
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(4),
        http(
            hub.addr,
            "POST",
            "/v1/instances",
            &[("Cookie", cookie.as_str())],
            Some(&request),
        ),
    )
    .await
    .expect("create must return before materialization");
    assert_eq!(status, 200, "{body}");
    assert!(started.elapsed() < Duration::from_secs(4));
    let created: Value = serde_json::from_str(body.trim()).expect("create json");
    assert_eq!(created["command"]["state"], json!("accepted"));
    assert_eq!(created["command"]["resolution"], json!("clear"));
    let command_id = created["command"]["commandId"]
        .as_str()
        .expect("commandId")
        .to_owned();
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();

    let interaction_id = remuda_protocol::InteractionId::new();
    for (operation, payload) in [
        ("instance.send", json!({"prompt": "queued-during-start"})),
        ("instance.cancel", json!({})),
        (
            "instance.respond",
            json!({
                "interactionId": interaction_id,
                "answer": {
                    "kind": "approval",
                    "optionId": "allow",
                    "inputDigest": format!("sha256:{}", "0".repeat(64))
                }
            }),
        ),
    ] {
        let command = json!({"operation": operation, "payload": payload}).to_string();
        let (status, body) = tokio::time::timeout(
            Duration::from_secs(4),
            http(
                hub.addr,
                "POST",
                &format!("/v1/instances/{instance_id}/commands"),
                &[("Cookie", cookie.as_str())],
                Some(&command),
            ),
        )
        .await
        .expect("command accept response");
        assert_eq!(status, 200, "{body}");
        let response: Value = serde_json::from_str(body.trim()).expect("command json");
        assert_eq!(response["command"]["state"], json!("accepted"));
        assert_eq!(response["command"]["resolution"], json!("clear"));
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
    let (status, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let during_start: Value = serde_json::from_str(body.trim()).expect("instance json");
    assert_eq!(during_start["connectivity"], json!("connected"));

    let journal_path = format!("/v1/instances/{instance_id}/journal");
    tokio::time::timeout(SLOW_TIMEOUT, async {
        loop {
            let (status, body) = http(
                hub.addr,
                "GET",
                &journal_path,
                &[("Cookie", cookie.as_str())],
                None,
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let journal: Value = serde_json::from_str(body.trim()).expect("journal json");
            if journal_has_command_state(&journal, &command_id, "settled") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("create settlement after slow start");

    let (status, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let settled: Value = serde_json::from_str(body.trim()).expect("instance json");
    assert_eq!(settled["connectivity"], json!("connected"));
    assert_ne!(settled["durableSeq"], json!("0"));

    link.shutdown().await;
    fake_herdr.shutdown().expect("fake herdr shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wss_create_preserves_gateway_delegation_overlay_and_budget() {
    use remuda_node::{DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, compose};
    use remuda_protocol::InstanceId;
    use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tmp");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .expect("hub");
    let mut node_http = DevServerConfig::loopback(0);
    node_http.workspace_root = workspace;
    node_http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let mut native = NativeDriverConfig::new(dir.path().join("node"))
        .with_claude_binary(ensure_workspace_bin("fake-claude"));
    native.extra_env.insert(
        "FAKE_CLAUDE_SCRIPT".to_owned(),
        script_path(ScriptKind::Ok).to_string_lossy().into_owned(),
    );
    let node = compose(&ServeConfig {
        http: node_http,
        data_dir: dir.path().join("node"),
        drivers: LocalDrivers::Native(native),
    })
    .expect("compose");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let mut config = WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id.clone());
    config.host = Some(json!({
        "hostname": "local-development",
        "labels": { "egress": "gateway" },
        "maxInstances": 8
    }));
    let query = node.clone();
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(config, node))
        .await
        .expect("connect timeout")
        .expect("wss runtime connect");
    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;
    let provider = json!({
        "name": "dummy-gateway",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "authToken": "sk-fake-test-gateway-token",
        "defaultGateway": true
    })
    .to_string();
    let (status, created_provider) = http(
        hub.addr,
        "POST",
        "/v1/providers",
        &[("Cookie", cookie.as_str())],
        Some(&provider),
    )
    .await;
    assert_eq!(status, 200, "{created_provider}");
    let created_provider: Value =
        serde_json::from_str(created_provider.trim()).expect("provider json");
    let profile_id = created_provider["id"].as_str().expect("provider id");

    let request = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-print",
        "model": "fake",
        "providerProfileId": profile_id,
        "permissionMode": "bypassPermissions",
        "delegation": "gateway",
        "maxBudgetUsd": "0.3",
        "prompt": "hello"
    })
    .to_string();
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(8),
        http(
            hub.addr,
            "POST",
            "/v1/instances",
            &[("Cookie", cookie.as_str())],
            Some(&request),
        ),
    )
    .await
    .expect("create timeout");
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(body.trim()).expect("create json");
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .expect("instanceId");
    assert_eq!(created["instance"]["delegation"], json!("gateway"));
    assert_eq!(created["instance"]["providerProfileId"], json!(profile_id));

    let instance_id = InstanceId::try_from(instance_id.to_owned()).expect("instance id");
    let recipe = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Some(recipe)) = query.launch_recipe(&instance_id) {
                break recipe;
            }
            if let Ok(instance) = query.get_instance(&instance_id)
                && instance.lifecycle == remuda_protocol::InstanceLifecycle::Failed
            {
                panic!("native start failed last_error={:?}", instance.last_error);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let listed = query.list_instances().expect("list");
        let instance = query.get_instance(&instance_id).ok();
        panic!(
            "recipe timeout listed={:?} instance={:?} last_error={:?}",
            listed
                .items
                .iter()
                .map(|row| (
                    row.meta.id.as_id().to_string(),
                    row.lifecycle,
                    row.last_error.clone()
                ))
                .collect::<Vec<_>>(),
            instance.as_ref().map(|row| row.lifecycle),
            instance.and_then(|row| row.last_error)
        );
    });
    assert_eq!(
        recipe.provider.delegation,
        remuda_driver::Delegation::Gateway
    );
    assert!(
        recipe.argv.windows(2).any(|pair| pair[0] == "--settings"),
        "WSS launch must write a gateway overlay: {:?}",
        recipe.argv
    );
    assert!(
        recipe
            .argv
            .windows(2)
            .any(|pair| pair[0] == "--max-budget-usd" && pair[1] == "0.3"),
        "WSS launch must keep the budget: {:?}",
        recipe.argv
    );

    link.shutdown().await;
}

/// D-028 §5.1 end-to-end proof over loopback WSS: the `driverInventory`
/// descriptor built from `REMUDA_PTY_CARRIER` rides the WSS hello into the Hub
/// host view. This is the path `remuda dev` and every real outbound Node use;
/// stdio was the only path that advertised it, so `GET /v1/hosts` showed
/// `capabilities: {}` and New Session defaulted back to the legacy herdr
/// carrier.
///
/// `REMUDA_PTY_CARRIER` is read from the process environment, which is unsafe
/// to mutate in-process (the workspace forbids `unsafe`, and the value would
/// leak to other tests sharing the process), so the two cases each run in a
/// re-exec of this test binary — the same pattern as
/// `child_env_isolation.rs`.
const HELLOCAPS_MARKER: &str = "REMUDA_HELLOCAPS_CHILD";
const HELLOCAPS_CASE: &str = "REMUDA_HELLOCAPS_CASE";
const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const HELLOCAPS_SENTINEL: &str = "HELLOCAPS_ASSERTED";

#[cfg(unix)]
#[test]
fn wss_hello_driver_inventory_reaches_hub_host_view() {
    for case in ["native", "legacy"] {
        let exe = std::env::current_exe().expect("test binary");
        let mut command = std::process::Command::new(exe);
        command
            .args([
                "--exact",
                "wss_driver_inventory_child",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(HELLOCAPS_MARKER, "1")
            .env(HELLOCAPS_CASE, case);
        if case == "native" {
            command.env(CARRIER_ENV, "native");
        } else {
            // Never inherit a flag from the developer's shell: the legacy case
            // must prove what a Node reports with the flag genuinely absent.
            command.env_remove(CARRIER_ENV);
        }
        let output = command.output().expect("re-exec the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{case} child failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "{case} inner test did not run: {stdout}"
        );
        assert!(
            stdout.contains(HELLOCAPS_SENTINEL),
            "{case} inner assertions did not execute: {stdout}"
        );
    }
}

/// The child half of [`wss_hello_driver_inventory_reaches_hub_host_view`]:
/// stands up the same Hub + collected-inventory WSS Node `remuda dev` does.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "re-executed with a controlled REMUDA_PTY_CARRIER"]
async fn wss_driver_inventory_child() {
    use remuda_node::{CollectRequest, DevNode, DevServerConfig, WssConfig, WssLink};

    let case = std::env::var(HELLOCAPS_CASE).expect("parent sets the case");
    assert!(
        std::env::var(HELLOCAPS_MARKER).is_ok(),
        "must be re-executed"
    );
    let expect_native = case == "native";
    assert_eq!(
        std::env::var(CARRIER_ENV).ok().as_deref(),
        expect_native.then_some("native"),
        "child carrier env must match the {case} case"
    );

    let dir = tempfile::tempdir().expect("tmp");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .expect("hub");
    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(workspace)
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let local_node = node.clone();

    // Mirror `remuda dev`: loopback config plus the live PATH/host probe,
    // which is what builds the nested `host.driverInventory`.
    let config = WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id.clone())
        .with_collected_inventory_from(&CollectRequest::default());
    assert!(
        config
            .host
            .as_ref()
            .and_then(|host| host.get("driverInventory"))
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty()),
        "collected host inventory must describe shell-pty before hello"
    );

    let link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(config, node))
        .await
        .expect("connect timeout")
        .expect("wss runtime connect");

    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;
    let (status, body) = http(
        hub.addr,
        "GET",
        "/v1/hosts",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let hosts: Value = serde_json::from_str(body.trim()).expect("hosts json");
    let host = hosts["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["hostId"] == json!(host_id))
        .unwrap_or_else(|| panic!("our host missing from {hosts}"));
    let descriptor = host["capabilities"]["driverInventory"]
        .as_array()
        .expect("capabilities.driverInventory")
        .iter()
        .find(|row| row["kind"] == json!("shell-pty"))
        .unwrap_or_else(|| panic!("shell-pty descriptor missing: {host}"));
    assert_eq!(
        descriptor["launchable"],
        json!(expect_native),
        "{case} host view: {host}"
    );
    if expect_native {
        assert_eq!(descriptor["reasonCode"], json!("carrier-native"));
    } else {
        // A non-launchable descriptor must say *why*; silence would leave the
        // UI unable to distinguish "off" from "unknown".
        assert_eq!(descriptor["reasonCode"], json!("carrier-not-enabled"));
    }

    // The DevNode's own Host record agrees, so the runtime and transport never
    // describe the host differently.
    let local_shell_pty = local_node
        .host()
        .driver_inventory
        .into_iter()
        .find(|row| matches!(row.kind, remuda_protocol::DriverKind::ShellPty))
        .expect("runtime host describes shell-pty");
    assert_eq!(local_shell_pty.launchable, expect_native);

    link.shutdown().await;
    hub.shutdown().await;
    println!("{HELLOCAPS_SENTINEL} case={case}");
}

fn journal_has_command_state(journal: &Value, command_id: &str, state: &str) -> bool {
    journal["events"].as_array().is_some_and(|events| {
        events.iter().any(|record| {
            let payload = &record["event"]["payload"];
            payload["entityType"] == "command"
                && payload["entityId"] == command_id
                && payload["state"] == state
        })
    })
}

/// The Node→Hub uplink must pipeline `journal.append` frames: a batch of N
/// events arrives within ~1 round trip, not N round trips. Before the
/// bounded-in-flight window the pump awaited one ACK per event, which on the
/// loaded demo host cost ~0.39 s median *per event* (2.4 s of a 2.8 s,
/// six-event batch — remuda-pipeline §2 hop 7).
///
/// A fake Hub adds a fixed independent per-frame delay to every append ACK.
/// We drive a real runtime Node whose shared FakeDriver emits one journal
/// event per `instance.send`, and measure from batch submission to the fake
/// Hub receiving the last frame. The assertion is relative: a batch of six
/// must not take more than ~one single-event RTT (with slack), so it fails
/// loudly if per-ACK serialization ever returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_uplink_pipelines_a_batch_within_one_rtt() {
    use futures::{SinkExt, StreamExt};
    use remuda_node::{DevNode, DevServerConfig, DriverRegistry, MemoryStore};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    const ACK_DELAY: Duration = Duration::from_millis(120);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake hub");
    let addr = listener.local_addr().expect("local addr");
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let arrivals = Arc::new(tokio::sync::Mutex::new(
        Vec::<(i64, tokio::time::Instant)>::new(),
    ));
    let arrivals_task = arrivals.clone();
    let server_ready = Arc::new(Notify::new());
    let server_ready_task = server_ready.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept node");
        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        server_ready_task.notify_one();
        let (mut sink, mut stream) = ws.split();
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if sink
                    .send(WsMsg::Text(frame.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        while let Some(Ok(WsMsg::Text(text))) = stream.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            match method {
                "node.hello" | "runtime.hello" => {
                    let _ = out_tx.send(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "nodeToken": "host-token", "hostId": "hst_fake" }
                    }));
                }
                "node.heartbeat" => {
                    let _ = out_tx.send(json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}}));
                }
                "journal.append" => {
                    let seq = frame["params"]["seq"]
                        .as_i64()
                        .or_else(|| frame["params"]["seq"].as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or(0);
                    arrivals_task
                        .lock()
                        .await
                        .push((seq, tokio::time::Instant::now()));
                    // Independent per-frame RTT — never wait on another frame.
                    let out_tx = out_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(ACK_DELAY).await;
                        let _ = out_tx.send(json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "seq": seq.to_string(), "durableSeq": seq.to_string(), "replayed": false }
                        }));
                    });
                }
                _ => {}
            }
        }
        let _ = writer.await;
    });

    let mut config = DevServerConfig::loopback(0);
    config.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let registry = DriverRegistry::default();
    registry
        .register(Arc::new(remuda_node::FakeDriver::new(
            remuda_protocol::DriverKind::ClaudePrint,
        )))
        .expect("register fake");
    let node =
        DevNode::with_parts(&config, Arc::new(MemoryStore::new(128)), registry).expect("node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();

    let created = node
        .create_instance(
            serde_json::from_value(json!({
                "kind": "claude", "driver": "claude-print", "prompt": ""
            }))
            .expect("request"),
        )
        .await
        .expect("create");
    let iid = created.instance.meta.id.clone();

    let mut ws_config = WssConfig::loopback(addr, "unused-enroll-token", host_id);
    ws_config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        jitter_ppt: 0,
    };
    ws_config.heartbeat_interval = Duration::from_secs(60);
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(ws_config, node.clone()))
        .await
        .expect("connect")
        .expect("runtime link");
    server_ready.notified().await;
    // hello triggers replay + pump start; let it drain the initial journal.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let send = |prompt: &'static str| {
        let node = node.clone();
        let iid = iid.clone();
        async move {
            node.submit_command(
                &iid,
                serde_json::from_value(json!({
                    "origin": "human", "operation": "send", "prompt": prompt
                }))
                .expect("request"),
            )
            .await
            .expect("submit");
        }
    };

    // Each send causes the FakeDriver's `execute` to journal an emission. We
    // measure from submission until the fake Hub observes a journal.append
    // frame, counting frames beyond the initial create replay.
    async fn wait_frames(
        arrivals: &Arc<tokio::sync::Mutex<Vec<(i64, tokio::time::Instant)>>>,
        n: usize,
    ) {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if arrivals.lock().await.len() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("frames delivered");
    }

    // Let the initial (create) replay settle and record the baseline count.
    tokio::time::sleep(ACK_DELAY + Duration::from_millis(200)).await;
    let mut base = arrivals.lock().await.len();
    eprintln!("baseline uplink frames: {base}");

    // Single event ≈ one RTT.
    let t_single = tokio::time::Instant::now();
    send("probe-single").await;
    wait_frames(&arrivals, base + 1).await;
    let single = arrivals.lock().await[base].1 - t_single;
    base += 1;
    tokio::time::sleep(ACK_DELAY + Duration::from_millis(100)).await;

    // Batch six; measure to the last frame received.
    let t_batch = tokio::time::Instant::now();
    for n in 0..6 {
        send(Box::leak(format!("probe-batch-{n}").into_boxed_str())).await;
    }
    wait_frames(&arrivals, base + 6).await;
    let batch = arrivals.lock().await[base + 5].1 - t_batch;

    eprintln!(
        "single={single:.0?} batch6={batch:.0?} ratio={:.2}",
        batch.as_secs_f64() / single.as_secs_f64()
    );
    // A serial uplink would spend ~6 RTT; pipelined must be ~1 RTT. Generous
    // slack so the relative assertion survives host load.
    assert!(
        batch < single * 2 + Duration::from_millis(150),
        "six-event batch took {batch:.0?} but one RTT is {single:.0?}; \
         the uplink is serializing on per-event ACKs again"
    );

    let _ = link.shutdown().await;
    server.abort();
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

/// Source: deterministic fake-claude and in-process Hub/Node, no model calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wss_authenticated_origin_parent_scope_and_one_shot_human_approval() {
    use remuda_node::{
        DevNode, DevServerConfig, MemoryStore, NativeDriverConfig, native_driver_registry,
    };
    use remuda_protocol::{ActorType, CommandId, CommandOrigin, CommandState};
    use remuda_testing::ensure_workspace_bin;
    use std::sync::Arc;

    async fn request(
        addr: std::net::SocketAddr,
        token: &str,
        path: &str,
        body: Option<Value>,
        approval: Option<&str>,
    ) -> (u16, Value) {
        let auth = format!("Bearer {token}");
        let mut headers = vec![("Authorization", auth.as_str())];
        if let Some(id) = approval {
            headers.push(("x-remuda-approval-id", id));
        }
        let body = body.map(|v| v.to_string());
        let (status, text) = http(
            addr,
            if body.is_some() { "POST" } else { "GET" },
            path,
            &headers,
            body.as_deref(),
        )
        .await;
        (
            status,
            serde_json::from_str(&text).unwrap_or(json!({"raw":text})),
        )
    }
    async fn settled(node: &DevNode, body: &Value) -> remuda_protocol::Command {
        let id: CommandId = body["command"]["commandId"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let command = node.get_command(&id).unwrap();
                if command.state == CommandState::Settled {
                    return command;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fake command settlement")
    }

    let dir = tempfile::tempdir().unwrap();
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // This authorization fixture must not depend on the developer's Claude
    // configuration, skills, or macOS permissions.
    let claude_config = dir.path().join("claude-config");
    std::fs::create_dir_all(&claude_config).unwrap();
    let config = DevServerConfig::loopback(0)
        .with_workspace_root(workspace)
        .with_workspace_roots(remuda_testing::test_workspace_roots!());
    let native = NativeDriverConfig::new(dir.path().join("node"))
        .with_claude_binary(ensure_workspace_bin("fake-claude"));
    let node = DevNode::with_parts(
        &config,
        Arc::new(MemoryStore::open_journaled(dir.path().join("node-store"), 512).unwrap()),
        native_driver_registry(native).unwrap(),
    )
    .unwrap();
    let host = node.host().meta.id.as_id().as_str().to_string();
    let link = WssLink::connect_runtime(
        WssConfig::loopback(hub.addr, enroll_token(&hub).await, host.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    let (_, human) = login(hub.addr, &hub.bootstrap_token).await;

    let (status, parent) = request(
        hub.addr,
        &human,
        "/v1/instances",
        Some(json!({"hostId":host,"driver":"claude-print","claudeConfigDir":claude_config,"permissionMode":"bypassPermissions","grants":["dispatch"],"prompt":"human-origin"})),
        None,
    )
    .await;
    assert_eq!(status, 200, "{parent}");
    let command = settled(&node, &parent).await;
    assert_eq!(command.origin, CommandOrigin::Ui);
    assert_eq!(command.actor.actor_type, ActorType::Human);
    let parent_id = parent["instance"]["instanceId"].as_str().unwrap();
    let recipe = node
        .launch_recipe(&parent_id.parse().unwrap())
        .unwrap()
        .unwrap_or_else(|| panic!("Human launch failed: {command:?}"));
    assert_eq!(
        recipe.permission.cli_mode.as_deref(),
        Some("bypassPermissions")
    );
    assert!(
        recipe
            .argv
            .iter()
            .any(|flag| flag == "--allow-dangerously-skip-permissions")
    );
    assert!(
        !serde_json::to_string(&parent)
            .unwrap()
            .contains("agentCredential")
    );

    let (status, issued) = request(
        hub.addr,
        &human,
        &format!("/v1/instances/{parent_id}/mcp-token"),
        Some(json!({})),
        None,
    )
    .await;
    assert_eq!(status, 200, "{issued}");
    let agent = issued["token"].as_str().unwrap();
    let (_, context) = request(hub.addr, agent, "/v1/caller", None, None).await;
    assert_eq!(context["origin"], "agent");
    assert_eq!(context["instanceId"], parent_id);

    let (status, denied_enrollment) = request(
        hub.addr,
        agent,
        "/v1/hosts/enroll-token",
        Some(json!({})),
        None,
    )
    .await;
    assert_eq!(status, 403, "{denied_enrollment}");

    let (status, child) = request(hub.addr, agent, "/v1/instances", Some(json!({"hostId":host,"driver":"claude-print","claudeConfigDir":claude_config,"origin":"human","parentInstanceId":"forged","prompt":"agent-origin"})), None).await;
    assert_eq!(status, 200, "{child}");
    assert_eq!(child["instance"]["parentInstanceId"], parent_id);
    assert_eq!(child["command"]["payload"]["origin"], "agent");
    let command = settled(&node, &child).await;
    assert_eq!(command.origin, CommandOrigin::Mcp);
    assert_eq!(command.actor.actor_type, ActorType::Agent);
    let child_id = child["instance"]["instanceId"].as_str().unwrap();
    let recipe = node
        .launch_recipe(&child_id.parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(recipe.permission.cli_mode.as_deref(), Some("default"));
    let serialized = serde_json::to_string(&recipe).unwrap();
    assert!(!serialized.contains(agent));
    assert!(!serialized.contains("REMUDA_TOKEN"));
    let (_, context) = request(hub.addr, agent, "/v1/caller", None, None).await;
    assert!(
        context["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == child_id)
    );

    for mode in [
        "bypassPermissions",
        "dontAsk",
        "auto",
        "acceptEdits",
        "default",
        "future",
    ] {
        let (status, denied_launch) = request(
            hub.addr,
            agent,
            "/v1/instances",
            Some(json!({"hostId":host,"driver":"claude-print","permissionMode":mode})),
            None,
        )
        .await;
        assert_eq!(status, 403, "Agent {mode}: {denied_launch}");
        assert!(
            denied_launch.get("instance").is_none(),
            "must reject before indexing or dispatch"
        );
        let (status, denied_fleet) = request(
            hub.addr,
            agent,
            "/v1/fleet/instances",
            Some(json!({"hosts":[host],"spec":{"permissionMode":mode}})),
            None,
        )
        .await;
        assert_eq!(status, 403, "Agent fleet {mode}: {denied_fleet}");
    }

    let (status, sibling) = request(
        hub.addr,
        &human,
        "/v1/instances",
        Some(json!({"hostId":host,"driver":"claude-print","claudeConfigDir":claude_config,"permissionMode":"manual"})),
        None,
    )
    .await;
    assert_eq!(status, 200, "{sibling}");
    settled(&node, &sibling).await;
    let sibling_id = sibling["instance"]["instanceId"].as_str().unwrap();
    for owned in [parent_id, child_id] {
        for suffix in ["", "/journal"] {
            let route = format!("/v1/instances/{owned}{suffix}");
            let (status, body) = request(hub.addr, agent, &route, None, None).await;
            assert_eq!(status, 200, "{route}: {body}");
        }
    }
    for target in [sibling_id, "ins_unrelated"] {
        for route in [
            format!("/v1/instances/{target}"),
            format!("/v1/instances/{target}/journal"),
            format!("/v1/instances?instanceId={target}"),
            format!("/v1/interactions?instanceId={target}"),
            format!("/v1/hosts/{target}"),
            format!("/v1/hosts/{target}/doctor"),
            format!("/v1/providers/{target}"),
            format!("/v1/fleet/{target}"),
        ] {
            let (status, body) = request(hub.addr, agent, &route, None, None).await;
            assert_eq!(status, 403, "{route}: {body}");
        }
    }
    // §2.5: this agent holds dispatch, so it may list instances — but only
    // its own subtree. The fleet-wide GET is no longer refused for it; the
    // sibling instance above must not appear.
    let (status, fleet) = request(hub.addr, agent, "/v1/instances", None, None).await;
    assert_eq!(status, 200, "{fleet}");
    let visible: Vec<&str> = fleet["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["instanceId"].as_str().unwrap())
        .collect();
    assert!(visible.contains(&parent_id));
    assert!(visible.contains(&child_id));
    assert!(!visible.contains(&sibling_id));
    for route in [
        "/v1/hosts",
        "/v1/devices",
        "/v1/providers",
        "/v1/interactions",
        "/v1/worktrees",
        "/v1/follow",
    ] {
        let (status, body) = request(hub.addr, agent, route, None, None).await;
        assert_eq!(status, 403, "{route}: {body}");
    }
    for route in [
        format!("/v1/hosts/{host}"),
        format!("/v1/hosts/{host}/doctor"),
    ] {
        assert_eq!(
            request(hub.addr, agent, &route, None, None).await.0,
            403,
            "{route}"
        );
    }
    // Human reads retain access to the same existing sibling.
    assert_eq!(
        request(
            hub.addr,
            &human,
            &format!("/v1/instances/{sibling_id}/journal"),
            None,
            None
        )
        .await
        .0,
        200
    );
    let path = format!("/v1/instances/{sibling_id}/commands");
    let command_id = CommandId::new();
    let send = json!({"commandId":command_id, "operation":"instance.send", "payload":{"instanceId":parent_id,"input":{"text":"approved-cross-send","origin":"human"},"origin":"human"}});
    let (status, held) = request(hub.addr, agent, &path, Some(send.clone()), None).await;
    assert_eq!(status, 409, "{held}");
    assert_eq!(held["code"], "HUMAN_APPROVAL_REQUIRED");
    assert!(
        node.get_command(&command_id).is_err(),
        "approval must precede dispatch"
    );
    let approval = held["interactionId"].as_str().unwrap();
    let (_, pending) = request(hub.addr, &human, "/v1/interactions", None, None).await;
    let ticket = pending["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["interactionId"] == approval)
        .unwrap();
    assert_eq!(ticket["state"], "pending");
    serde_json::from_value::<remuda_protocol::Interaction>(ticket.clone())
        .expect("schema-valid broker interaction");
    let answer = json!({"answer":{"kind":"approval","optionId":"allow-once","inputDigest":ticket["request"]["inputDigest"]}});
    let answer_path = format!("/v1/interactions/{approval}/answer");
    assert_eq!(
        request(hub.addr, agent, &answer_path, Some(answer.clone()), None)
            .await
            .0,
        403
    );
    let (status, answered) =
        request(hub.addr, &human, &answer_path, Some(answer.clone()), None).await;
    assert_eq!(status, 200, "{answered}");
    assert_eq!(
        request(hub.addr, &human, &answer_path, Some(answer), None)
            .await
            .0,
        409,
        "first answer wins"
    );
    let mut changed = send.clone();
    changed["payload"]["input"]["text"] = json!("different action");
    assert_eq!(
        request(hub.addr, agent, &path, Some(changed), Some(approval))
            .await
            .0,
        403
    );
    let (status, sent) = request(hub.addr, agent, &path, Some(send.clone()), Some(approval)).await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(sent["command"]["payload"]["instanceId"], sibling_id);
    assert_eq!(settled(&node, &sent).await.origin, CommandOrigin::Mcp);
    assert_eq!(
        request(hub.addr, agent, &path, Some(send), Some(approval))
            .await
            .0,
        403,
        "approval cannot be replayed"
    );

    let child_path = format!("/v1/instances/{child_id}/commands");
    let (status, sent) = request(
        hub.addr,
        agent,
        &child_path,
        Some(json!({"operation":"instance.send","payload":{"prompt":"own-child"}})),
        None,
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(settled(&node, &sent).await.origin, CommandOrigin::Mcp);
    let (status, sent) = request(hub.addr, &human, &child_path, Some(json!({"operation":"instance.send","payload":{"prompt":"human-after-agent","origin":"agent"}})), None).await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(settled(&node, &sent).await.origin, CommandOrigin::Ui);
    assert_eq!(
        request(
            hub.addr,
            agent,
            &child_path,
            Some(json!({"operation":"tty.write","payload":{"keys":["enter"]}})),
            None
        )
        .await
        .0,
        409,
        "keys require approval even for a child"
    );
    assert_eq!(
        request(
            hub.addr,
            agent,
            "/v1/devices/pair-code",
            Some(json!({})),
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(
            hub.addr,
            agent,
            &format!("/v1/instances/{child_id}/mcp-token"),
            Some(json!({})),
            None
        )
        .await
        .0,
        403
    );

    // A terminal's initial prompt is raw input too: aliases cannot bypass the
    // same approval gate by using create instead of keys.
    let before_shell = node.list_instances().unwrap().items.len();
    for alias in ["shell-pty", "shell", "terminal"] {
        let (status, held) = request(
            hub.addr,
            agent,
            "/v1/instances",
            Some(json!({"hostId":host,"kind":"terminal","driver":alias,"prompt":"echo held"})),
            None,
        )
        .await;
        assert_eq!(status, 409, "{held}");
        assert_eq!(held["code"], "HUMAN_APPROVAL_REQUIRED");
    }
    assert_eq!(node.list_instances().unwrap().items.len(), before_shell);

    let remote_id = HostId::new().as_id().as_str().to_string();
    let remote = WssLink::connect(WssConfig::loopback(
        hub.addr,
        enroll_token(&hub).await,
        remote_id.clone(),
    ))
    .await
    .unwrap();
    let (status, cross_create) = request(
        hub.addr,
        agent,
        "/v1/instances",
        Some(json!({"hostId":remote_id,"driver":"claude-print"})),
        None,
    )
    .await;
    assert_eq!(status, 409, "{cross_create}");
    assert_eq!(cross_create["code"], "HUMAN_APPROVAL_REQUIRED");

    let (status, bot_login) = http(hub.addr, "POST", "/v1/login", &[], Some(&json!({"bootstrapToken":hub.bootstrap_token,"deviceName":"dispatcher","deviceKind":"bot"}).to_string())).await;
    assert_eq!(status, 200, "{bot_login}");
    let bot_login: Value = serde_json::from_str(&bot_login).unwrap();
    let bot = bot_login["token"].as_str().unwrap();
    let (status, bot_create) = request(
        hub.addr,
        bot,
        "/v1/instances",
        Some(json!({"hostId":host,"driver":"claude-print","claudeConfigDir":claude_config,"permissionMode":"bypassPermissions"})),
        None,
    )
    .await;
    assert_eq!(status, 200, "{bot_create}");
    let bot_command = settled(&node, &bot_create).await;
    assert_eq!(bot_command.origin, CommandOrigin::Bot);
    assert!(
        serde_json::to_string(&bot_command.settlement)
            .unwrap()
            .contains("not allowed")
    );

    let dispatcher = hub.mint_bot_device_token("dispatcher").await.unwrap();
    let (status, sent) = request(
        hub.addr,
        &dispatcher,
        &child_path,
        Some(
            json!({"operation":"instance.send","payload":{"prompt":"bot-origin","origin":"human"}}),
        ),
        None,
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    let command = settled(&node, &sent).await;
    assert_eq!(command.origin, CommandOrigin::Bot);
    assert_eq!(command.actor.actor_type, ActorType::Bot);

    for id in [parent_id, child_id, sibling_id] {
        let (_, closed) = request(
            hub.addr,
            &human,
            &format!("/v1/instances/{id}/commands"),
            Some(json!({"operation":"instance.close"})),
            None,
        )
        .await;
        settled(&node, &closed).await;
    }
    remote.shutdown().await;
    link.shutdown().await;
    hub.shutdown().await;
}

/// A runtime Node must announce its instance inventory on **every** hello, not
/// only when it is a daemon.
///
/// Without it the Hub sees a new `nodeEpoch` with nothing to compare against and
/// refuses to reconcile (it must not wipe rows a stateless Node simply never
/// enumerates), so instances the Node lost stay `running` forever and keep
/// holding placement slots — the zombie wedge seen on the demo.
#[tokio::test]
async fn runtime_hello_always_reports_the_instance_inventory() {
    use remuda_node::{DevNode, DevServerConfig};

    let dir = tempfile::tempdir().expect("tmp");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("data")))
        .await
        .expect("hub");
    let node = DevNode::new(
        &DevServerConfig::loopback(0)
            .with_workspace_root(dir.path().to_path_buf())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("dev node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let mut config = WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id.clone());
    config.backoff = Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        jitter_ppt: 0,
    };

    let link = tokio::time::timeout(
        TIMEOUT,
        WssLink::connect_runtime(config.clone(), node.clone()),
    )
    .await
    .expect("connect timeout")
    .expect("runtime connect");
    let token = link
        .node_token
        .clone()
        .expect("first hello returns a host token");

    let (cookie, _) = login(hub.addr, &hub.bootstrap_token).await;
    let create = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-print",
        "model": "haiku",
        "prompt": "inventory"
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
    let known = created["instance"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();

    // Take the link down, then create a second instance the Node can never
    // learn about: the Hub indexes the row, but the create is not forwarded.
    tokio::time::timeout(TIMEOUT, link.shutdown())
        .await
        .expect("shutdown timeout");
    let (status, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(&create),
    )
    .await;
    let orphan = match status {
        200 => serde_json::from_str::<Value>(body.trim())
            .ok()
            .and_then(|value| value["instance"]["instanceId"].as_str().map(str::to_string)),
        _ => None,
    };

    // Reconnect as a plain runtime Node — no daemon controller anywhere — so
    // the reconcile can only work if a non-daemon hello carries `instances`.
    config.token = token;
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(config, node.clone()))
        .await
        .expect("reconnect timeout")
        .expect("reannounce");

    let instance_state = |id: String| {
        let cookie = cookie.clone();
        let addr = hub.addr;
        async move {
            let (status, body) = http(
                addr,
                "GET",
                &format!("/v1/instances/{id}"),
                &[("Cookie", cookie.as_str())],
                None,
            )
            .await;
            assert_eq!(status, 200, "{body}");
            serde_json::from_str::<Value>(body.trim()).expect("instance json")
        }
    };

    // The instance the Node still holds must survive the reconcile.
    let kept = instance_state(known.clone()).await;
    assert_ne!(
        kept["lifecycle"], "exited",
        "a reported instance must survive the reconcile: {kept}"
    );

    // The one it never received must be reconciled away, which only happens
    // when the hello actually carried an inventory to compare against.
    if let Some(orphan) = orphan {
        let row = instance_state(orphan.clone()).await;
        assert_eq!(
            row["lifecycle"], "exited",
            "an instance the node never received must be reconciled: {row}"
        );
        assert_eq!(row["lastError"], "node-epoch-changed", "{row}");
    }

    link.shutdown().await;
    hub.shutdown().await;
}
