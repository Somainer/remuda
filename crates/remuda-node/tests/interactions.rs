//! In-process Hub + Node: FakeDriver `can_use_tool` → HTTP answer → driver receipt.

use remuda_hub::HubConfig;
use remuda_node::{DevNode, DevServerConfig, WssConfig, WssLink, attach_runtime};
use remuda_protocol::CommandId;
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

fn cookie_from(text: &str) -> Option<String> {
    for line in text.lines() {
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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> String {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "interaction-test"
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
    cookie_from(&text).expect("set-cookie")
}

#[tokio::test]
async fn fake_can_use_tool_answered_via_hub_http() {
    let dir = tempfile::tempdir().expect("tmp");
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .expect("hub");
    let node = DevNode::new(
        &DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!()),
    )
    .expect("node");
    let host_id = node.host().meta.id.as_id().as_str().to_owned();
    let config = WssConfig::loopback(hub.addr, enroll_token(&hub).await, host_id);
    let link = tokio::time::timeout(TIMEOUT, WssLink::connect(config))
        .await
        .expect("connect timeout")
        .expect("wss connect");
    let runtime = node.clone();
    tokio::spawn(async move {
        let _ = attach_runtime(link, runtime).await;
    });

    let cookie = login(hub.addr, &hub.bootstrap_token).await;
    let create = json!({
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "prompt": "can_use_tool"
    })
    .to_string();
    let (status, created) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &[("Cookie", cookie.as_str())],
        Some(&create),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let created: Value = serde_json::from_str(created.trim()).expect("create json");
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .or_else(|| created["instance"]["id"].as_str())
        .expect("instance id")
        .to_owned();

    let pending = tokio::time::timeout(TIMEOUT, async {
        loop {
            let (status, body) = http(
                hub.addr,
                "GET",
                "/v1/interactions?kind=approval",
                &[("Cookie", cookie.as_str())],
                None,
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let json: Value = serde_json::from_str(body.trim()).expect("list json");
            if json["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
            {
                break json;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("pending interaction");

    let item = &pending["items"][0];
    let interaction_id = item["interactionId"]
        .as_str()
        .or_else(|| item["interaction"]["id"].as_str())
        .expect("interaction id");
    let digest = item["interaction"]["request"]["inputDigest"]
        .as_str()
        .expect("input digest");
    let command_id = CommandId::new();
    let answer = json!({
        "commandId": command_id.as_id().as_str(),
        "answer": {
            "kind": "approval",
            "optionId": "allow",
            "inputDigest": digest
        }
    })
    .to_string();
    let path = format!("/v1/interactions/{interaction_id}/answer");
    let (status, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", cookie.as_str())],
        Some(&answer),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let answered: Value = serde_json::from_str(body.trim()).expect("answer json");
    assert_eq!(answered["outcome"], json!("accepted"));

    let (status, listed) = http(
        hub.addr,
        "GET",
        "/v1/interactions",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    let listed: Value = serde_json::from_str(listed.trim()).expect("list after");
    assert!(
        listed["items"]
            .as_array()
            .is_some_and(|items| items.is_empty()),
        "pending list should clear after answer: {listed}"
    );

    let other = CommandId::new();
    let again = json!({
        "commandId": other.as_id().as_str(),
        "answer": {
            "kind": "approval",
            "optionId": "allow",
            "inputDigest": digest
        }
    })
    .to_string();
    let (status, superseded) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", cookie.as_str())],
        Some(&again),
    )
    .await;
    assert!(
        status == 409 || status == 404,
        "second answer should be superseded or gone: {status} {superseded}"
    );

    let journal_path = format!("/v1/instances/{instance_id}/journal");
    let journal = tokio::time::timeout(TIMEOUT, async {
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
            if body.contains("fake-interaction") || body.contains("interaction.answered") {
                break body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("driver receipt in journal");
    assert!(
        journal.contains("fake-interaction") || journal.contains("recorded response"),
        "driver should record the answer: {journal}"
    );
}

/// Handwritten fake-herdr approval through the real native driver and Hub broker.
#[tokio::test]
async fn pty_approval_hub_cas_settlement_and_restart_do_not_replay() {
    use remuda_node::{MemoryStore, NativeDriverConfig, native_driver_registry};
    use remuda_testing::{
        FakeHerdrOptions, FakeHerdrScript, FakeHerdrServer, ensure_workspace_bin,
        install_executable,
    };
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let socket_dir = dir.path().join("herdr");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let mut fake_options = FakeHerdrOptions::new(socket_dir.join("herdr.sock"));
    fake_options.script = FakeHerdrScript::Approval;
    let _fake = FakeHerdrServer::spawn(fake_options).unwrap();
    let binary = install_executable(dir.path(), "claude", "#!/bin/sh\necho 'stub 1.0'\n");
    let mut config = NativeDriverConfig::new(dir.path().join("native")).with_claude_binary(binary);
    config.herdr_socket_dir = Some(socket_dir);
    config.herdr_binary = Some(ensure_workspace_bin("fake-herdr"));
    let node_path = dir.path().join("node");
    let store = Arc::new(MemoryStore::open_journaled(&node_path, 128).unwrap());
    let mut node_config = DevServerConfig::loopback(0);
    node_config.workspace_root = dir.path().to_path_buf();
    node_config.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let node = DevNode::with_parts(
        &node_config,
        store.clone(),
        native_driver_registry(config).unwrap(),
    )
    .unwrap();
    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .unwrap();
    let host = node.host().meta.id.as_id().to_string();
    let link = WssLink::connect_runtime(
        WssConfig::loopback(hub.addr, enroll_token(&hub).await, host.clone()),
        node.clone(),
    )
    .await
    .unwrap();
    let _link = link;
    let cookie = login(hub.addr, &hub.bootstrap_token).await;
    let (status, body) = http(hub.addr, "POST", "/v1/instances", &[("Cookie", &cookie)], Some(&json!({"hostId":host,"kind":"claude","driver":"generic-pty","delegation":"none","cwd":dir.path(),"prompt":""}).to_string())).await;
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    let instance = created["instance"]["instanceId"]
        .as_str()
        .or_else(|| created["instance"]["id"].as_str())
        .unwrap();
    let item = tokio::time::timeout(TIMEOUT, async {
        loop {
            let (_, body) = http(
                hub.addr,
                "GET",
                &format!("/v1/interactions?instanceId={instance}"),
                &[("Cookie", &cookie)],
                None,
            )
            .await;
            let page: Value = serde_json::from_str(&body).unwrap();
            if let Some(item) = page["items"].as_array().and_then(|items| items.first()) {
                break item.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if item.is_err() {
        let (_, journal) = http(
            hub.addr,
            "GET",
            &format!("/v1/instances/{instance}/journal"),
            &[("Cookie", &cookie)],
            None,
        )
        .await;
        panic!("no interaction; journal={journal}");
    }
    let item = item.unwrap();
    assert_eq!(item["instanceId"], instance);
    assert_eq!(item["carrier"], "native-tty");
    assert!(
        item["request"]["description"]
            .as_str()
            .unwrap()
            .contains("[y/N]")
    );
    let id = item["id"].as_str().unwrap();
    let path = format!("/v1/interactions/{id}/answer");
    let command = CommandId::new().as_id().to_string();
    let reply = json!({"commandId":command,"answer":{"kind":"approval","optionId":"y","inputDigest":item["request"]["inputDigest"]}});
    let mut invalid = reply.clone();
    invalid["answer"]["optionId"] = json!("unoffered-option");
    let (status, _) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&invalid.to_string()),
    )
    .await;
    assert_eq!(status, 400, "invalid answer cannot consume CAS");
    let (status, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&reply.to_string()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["outcome"],
        "accepted"
    );
    let (status, body) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&reply.to_string()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["outcome"],
        "idempotent"
    );
    let mut losing = reply.clone();
    losing["commandId"] = json!(CommandId::new().as_id().as_str());
    let (status, _) = http(
        hub.addr,
        "POST",
        &path,
        &[("Cookie", &cookie)],
        Some(&losing.to_string()),
    )
    .await;
    assert_eq!(status, 409);
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let (_, body) = http(
                hub.addr,
                "GET",
                &format!("/v1/instances/{instance}/journal"),
                &[("Cookie", &cookie)],
                None,
            )
            .await;
            if body.contains("native-cleared") && body.contains("resolved") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("native settlement mirrored");
    let (_, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/interactions?instanceId={instance}"),
        &[("Cookie", &cookie)],
        None,
    )
    .await;
    assert!(
        serde_json::from_str::<Value>(&body).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let restarted = remuda_node::InteractionRuntime::spawn(store.clone()).unwrap();
    assert!(
        restarted.list(None, None).await.is_empty(),
        "settled ticket must not resurrect on restart"
    );
    hub.shutdown().await;
}
