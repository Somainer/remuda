//! ma-lineage round 2 (item 6): after a continuation resume, the successor
//! chapter drives a REAL fake-harness `claude-sdk` turn — not merely an HTTP
//! send the fake Node accepted.
//!
//! Before this case the continuation suite only observed a forwarded
//! `instance.send`; here a real in-process Node (native driver registry,
//! `fake-claude` on PATH, the `twoturn` script) is connected to the in-process
//! Hub over the outbound WS link, so `--resume` actually launches a second
//! long-lived child and two subsequent prompts produce two real turns in it.

use remuda_hub::HubConfig;
use remuda_node::{
    Backoff, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, WssConfig, WssLink,
    compose,
};
use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(30);

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str(&format!("Cookie: {cookie}\r\n\r\n"));
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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> String {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "lineage-sdk" }).to_string();
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    let req = format!(
        "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let cookie = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split(':').nth(1))
        .map(|value| value.trim().split(';').next().unwrap().trim().to_owned())
        .expect("set-cookie");
    let json: Value = serde_json::from_str(rest.trim()).expect("login json");
    assert!(json["token"].is_string());
    cookie
}

async fn enroll_token(hub: &remuda_hub::RunningHub) -> String {
    hub.mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .expect("mint enroll token")
}

async fn wait_for<F>(addr: std::net::SocketAddr, cookie: &str, path: &str, mut pred: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let (status, body) = http(addr, "GET", path, cookie, None).await;
            assert_eq!(status, 200, "{body}");
            let value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
            if pred(&value) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("condition within timeout")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_claude_sdk_successor_drives_two_real_fake_harness_turns() {
    let dir = tempfile::tempdir().expect("tmp");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub")))
        .await
        .expect("hub");

    let mut node_http = DevServerConfig::loopback(0);
    node_http.workspace_root = workspace.clone();
    node_http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let mut native = NativeDriverConfig::new(dir.path().join("node"))
        .with_claude_binary(ensure_workspace_bin("fake-claude"));
    // Two-turn long-lived child: proves the resumed sdk process survives more
    // than one turn (the `{"turn":"end"}` barriers in the script).
    native.extra_env.insert(
        "FAKE_CLAUDE_SCRIPT".to_owned(),
        script_path(ScriptKind::TwoTurn)
            .to_string_lossy()
            .into_owned(),
    );
    let node = compose(&ServeConfig {
        http: node_http,
        data_dir: dir.path().join("node"),
        drivers: LocalDrivers::Native(native),
    })
    .expect("compose");
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
    let cookie = login(hub.addr, &hub.bootstrap_token).await;

    // A gateway profile the materializer attaches; fake-claude never calls it.
    let provider = json!({
        "name": "dummy-gateway",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "authToken": "sk-fake-test-gateway-token",
        "defaultGateway": true
    })
    .to_string();
    let (status, body) = http(hub.addr, "POST", "/v1/providers", &cookie, Some(&provider)).await;
    assert_eq!(status, 200, "{body}");
    let profile: Value = serde_json::from_str(body.trim()).expect("provider json");
    let profile_id = profile["id"].as_str().expect("provider id");

    // Chapter 1: a continuity claude-sdk seat (grant + restart policy) with an
    // initial prompt.
    let create = json!({
        "hostId": host_id,
        "kind": "claude",
        "driver": "claude-sdk",
        "model": "fake",
        "providerProfileId": profile_id,
        "permissionMode": "manual",
        "grants": ["address-owner"],
        "restart": { "onProcessLoss": true, "maxPerHour": 3 },
        "prompt": "first"
    })
    .to_string();
    let (status, body) = http(hub.addr, "POST", "/v1/instances", &cookie, Some(&create)).await;
    assert_eq!(status, 200, "{body}");
    let created: Value = serde_json::from_str(body.trim()).expect("create json");
    let x = created["instance"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();

    // The real child runs its first turn and reports its native session.
    let x_view = wait_for(hub.addr, &cookie, &format!("/v1/instances/{x}"), |view| {
        view["nativeSessionId"].is_string() && view["activity"] == json!("idle")
    })
    .await;
    let session_id = x_view["nativeSessionId"].as_str().unwrap().to_owned();

    // Continuation resume: fence X, launch Y with --resume in ONE transaction.
    let (status, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{x}/resume"),
        &cookie,
        Some(r#"{"mode":"structured"}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let resumed: Value = serde_json::from_str(body.trim()).expect("resume json");
    let y = resumed["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(x, y);
    assert_eq!(resumed["instance"]["lineageId"], json!(x));
    assert_eq!(resumed["instance"]["generation"], json!(2));
    assert_eq!(resumed["instance"]["driver"], json!("claude-sdk"));
    // The forwarded create really is a native --resume of X's session.
    assert_eq!(resumed["command"]["operation"], json!("instance.resume"));
    assert_eq!(
        resumed["command"]["payload"]["spec"]["resumeSessionId"],
        json!(session_id)
    );

    // Wait for the successor's real child to come up and settle.
    let _ = wait_for(hub.addr, &cookie, &format!("/v1/instances/{y}"), |view| {
        view["lifecycle"] == json!("running")
    })
    .await;

    // First turn on the RESUMED chapter: a real fake-harness turn.
    let send = json!({
        "operation": "instance.send",
        "payload": { "input": { "type": "prompt", "text": "again-one" } }
    })
    .to_string();
    let (status, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{y}/commands"),
        &cookie,
        Some(&send),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let _ = wait_for(
        hub.addr,
        &cookie,
        &format!("/v1/instances/{y}/journal"),
        |journal| journal.to_string().contains("msg_fake_sdk_turn1"),
    )
    .await;

    // Second turn in the SAME resumed process — the property only the real
    // long-lived sdk child has.
    let send = json!({
        "operation": "instance.send",
        "payload": { "input": { "type": "prompt", "text": "again-two" } }
    })
    .to_string();
    let (status, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/instances/{y}/commands"),
        &cookie,
        Some(&send),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let journal = wait_for(
        hub.addr,
        &cookie,
        &format!("/v1/instances/{y}/journal"),
        |journal| journal.to_string().contains("msg_fake_sdk_turn2"),
    )
    .await;

    // Both turns are assistant message observations on the successor chapter.
    let text = journal.to_string();
    assert!(
        text.contains("First."),
        "first resumed turn missing: {text}"
    );
    assert!(
        text.contains("Second."),
        "second resumed turn missing: {text}"
    );

    link.shutdown().await;
}
