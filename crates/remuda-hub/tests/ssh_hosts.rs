//! Hub supervision contract; fake ssh runs a local deterministic NDJSON Node.
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, RunningHub};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

async fn assert_bootstrap_cannot_claim_ssh_host(hub: &RunningHub, id: &str) {
    let mut request = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", hub.bootstrap_token).parse().unwrap(),
    );
    let mut node = match tokio_tungstenite::connect_async(request).await {
        Ok((node, _)) => node,
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if matches!(response.status().as_u16(), 401 | 403) =>
        {
            return;
        }
        Err(error) => panic!("unexpected Node upgrade failure: {error}"),
    };
    node.send(Message::Text(
        json!({"jsonrpc":"2.0","id":"claim","method":"node.hello","params":{"hostId":id}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), node.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert!(response.get("error").is_some(), "{response}");
    node.close(None).await.unwrap();
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (u16, Value) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.to_string();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}
async fn login(hub: &RunningHub) -> String {
    let (status, body) = request(
        hub.addr,
        "POST",
        "/v1/login",
        "",
        json!({"bootstrapToken":hub.bootstrap_token,"deviceName":"SSH test"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["token"].as_str().unwrap().into()
}
async fn wait_host(
    hub: &RunningHub,
    token: &str,
    id: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (_, host) = request(
                hub.addr,
                "GET",
                &format!("/v1/hosts/{id}"),
                token,
                Value::Null,
            )
            .await;
            if predicate(&host) {
                return host;
            }
            last = host;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("host {id} status deadline: {last}"))
}
fn executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
fn fake_ssh(root: &Path, node: &Path) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::os::unix::fs::symlink(node, bin.join("remuda")).unwrap();
    let ssh = root.join("ssh");
    executable(
        &ssh,
        &format!(
            "#!/bin/sh\nexport PATH='{}:/usr/bin:/bin'\nexport HOME='{}'\nfor arg do command=$arg; done\nexec /bin/sh -c \"$command\"\n",
            bin.display(),
            root.join("home").display()
        ),
    );
    ssh
}

fn fixture_pid(id: &str) -> String {
    std::fs::read_to_string(format!("/tmp/remuda-ssh-{id}/fixture-pid")).unwrap()
}

async fn assert_process_stopped(pid: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap()
            .status
            .success()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("SSH child must stop with its supervisor");
}

#[tokio::test]
async fn api_supervises_reconnect_restart_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("node");
    executable(&fixture, include_str!("fixtures/ssh/fake-node.py"));
    let mut config = HubConfig::for_test(dir.path().join("hub"));
    config.ssh_hosts.ssh_binary = fake_ssh(dir.path(), &fixture);
    let hub = remuda_hub::spawn(config.clone()).await.unwrap();
    let token = login(&hub).await;
    let body = json!({"target":"test-node","label":"SG worker","labels":["egress:gateway"],"remuda_binary_policy":"require_installed"});
    let (status, _) = request(hub.addr, "POST", "/v1/hosts/ssh", "invalid", body.clone()).await;
    assert_eq!(status, 401);
    let (status, added) = request(hub.addr, "POST", "/v1/hosts/ssh", &token, body.clone()).await;
    assert_eq!(status, 201, "{added}");
    assert_eq!(added["state"], "connecting");
    let id = added["id"].as_str().unwrap();
    let remote = PathBuf::from(format!("/tmp/remuda-ssh-{id}"));
    let host = wait_host(&hub, &token, id, |h| h["online"] == true).await;
    assert_eq!(host["hostname"], "fixture-node");
    assert_eq!(host["labels"][0], "egress=gateway");
    assert_eq!(host["label"], "SG worker");
    assert!(host["lastError"].is_null());
    assert_bootstrap_cannot_claim_ssh_host(&hub, id).await;
    let (status, _) = request(hub.addr, "POST", "/v1/hosts/ssh", &token, body).await;
    assert_eq!(status, 409);
    let (status, worktrees) = request(
        hub.addr,
        "GET",
        &format!("/v1/worktrees?hostId={id}"),
        &token,
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{worktrees}");
    assert_eq!(worktrees["fixtureHostId"], id);
    let pid = std::fs::read_to_string(remote.join("fixture-pid")).unwrap();
    std::process::Command::new("kill")
        .args(["-TERM", pid.trim()])
        .status()
        .unwrap();
    wait_host(&hub, &token, id, |h| h["lastError"].is_string()).await;
    wait_host(&hub, &token, id, |h| {
        h["online"] == true && h["lastError"].is_null()
    })
    .await;
    // Display names and reported OS hostnames are not SSH target identities.
    let (status, other) = request(
        hub.addr,
        "POST",
        "/v1/hosts/ssh",
        &token,
        json!({"target":"second-node","label":"SG worker"}),
    )
    .await;
    assert_eq!(status, 201, "{other}");
    let other_id = other["id"].as_str().unwrap();
    wait_host(&hub, &token, other_id, |h| h["online"] == true).await;
    let stopped_on_shutdown = [fixture_pid(id), fixture_pid(other_id)];
    hub.shutdown().await;
    for pid in stopped_on_shutdown {
        assert_process_stopped(&pid).await;
    }
    let hub = remuda_hub::spawn(config).await.unwrap();
    let token = login(&hub).await;
    wait_host(&hub, &token, id, |h| h["online"] == true).await;
    wait_host(&hub, &token, other_id, |h| h["online"] == true).await;
    let (_, hosts) = request(hub.addr, "GET", "/v1/hosts", &token, Value::Null).await;
    assert_eq!(hosts["items"].as_array().unwrap().len(), 2);
    let stopped_on_removal = [fixture_pid(id), fixture_pid(other_id)];
    assert_eq!(
        request(
            hub.addr,
            "DELETE",
            &format!("/v1/hosts/{other_id}"),
            &token,
            Value::Null
        )
        .await
        .0,
        204
    );
    let (status, _) = request(
        hub.addr,
        "DELETE",
        &format!("/v1/hosts/{id}"),
        &token,
        Value::Null,
    )
    .await;
    assert_eq!(status, 204);
    for pid in stopped_on_removal {
        assert_process_stopped(&pid).await;
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, hosts) = request(hub.addr, "GET", "/v1/hosts", &token, Value::Null).await;
    assert_eq!(hosts["items"], json!([]));
    hub.shutdown().await;
    std::fs::remove_dir_all(remote).unwrap();
    std::fs::remove_dir_all(format!("/tmp/remuda-ssh-{other_id}")).unwrap();
}

#[tokio::test]
async fn drop_and_failed_bind_cancel_ssh_supervision() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("node");
    executable(&fixture, include_str!("fixtures/ssh/fake-node.py"));
    let mut config = HubConfig::for_test(dir.path().join("hub"));
    config.ssh_hosts.ssh_binary = fake_ssh(dir.path(), &fixture);
    let hub = remuda_hub::spawn(config.clone()).await.unwrap();
    let token = login(&hub).await;
    let (status, added) = request(
        hub.addr,
        "POST",
        "/v1/hosts/ssh",
        &token,
        json!({"target":"drop-node","label":"drop test"}),
    )
    .await;
    assert_eq!(status, 201, "{added}");
    let id = added["id"].as_str().unwrap();
    wait_host(&hub, &token, id, |h| h["online"] == true).await;
    let pid = fixture_pid(id);
    drop(hub);
    assert_process_stopped(&pid).await;

    let remote = PathBuf::from(format!("/tmp/remuda-ssh-{id}"));
    std::fs::remove_file(remote.join("fixture-pid")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.listen = listener.local_addr().unwrap();
    assert!(remuda_hub::spawn(config).await.is_err());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    if remote.join("fixture-pid").exists() {
        assert_process_stopped(&fixture_pid(id)).await;
    }
    std::fs::remove_dir_all(remote).unwrap();
}

#[tokio::test]
async fn invalid_and_missing_binary_report_actionable_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join("ssh");
    executable(
        &ssh,
        "#!/bin/sh\nfor arg do command=$arg; done\ncase \"$command\" in *'uname -s'*) printf 'Linux\\nx86_64\\n';; *) exec /bin/sh -c \"$command\";; esac\n",
    );
    let mut config = HubConfig::for_test(dir.path().join("hub"));
    config.ssh_hosts.ssh_binary = ssh;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let token = login(&hub).await;
    for target in ["-oProxyCommand=id", "a;id", "user@@host"] {
        assert_eq!(
            request(
                hub.addr,
                "POST",
                "/v1/hosts/ssh",
                &token,
                json!({"target":target,"label":"bad"})
            )
            .await
            .0,
            400
        );
    }
    let (_, added) = request(
        hub.addr,
        "POST",
        "/v1/hosts/ssh",
        &token,
        json!({"target":"missing-node","label":"missing"}),
    )
    .await;
    let id = added["id"].as_str().unwrap();
    wait_host(&hub, &token, id, |h| {
        h["lastError"]
            .as_str()
            .is_some_and(|e| e.contains("upload_if_missing"))
    })
    .await;
    assert_eq!(
        request(
            hub.addr,
            "DELETE",
            &format!("/v1/hosts/{id}"),
            &token,
            Value::Null
        )
        .await
        .0,
        204
    );
    hub.shutdown().await;
    let _ = std::fs::remove_dir_all(format!("/tmp/remuda-ssh-{id}"));
}

#[tokio::test]
async fn bridge_loss_preserves_instance_then_replays_from_hub_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("node");
    executable(&fixture, include_str!("fixtures/ssh/fake-node.py"));
    let mut config = HubConfig::for_test(dir.path().join("hub"));
    config.ssh_hosts.ssh_binary = fake_ssh(dir.path(), &fixture);
    config.host_lost_grace_ms = 25;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let token = login(&hub).await;
    let (status, added) = request(
        hub.addr,
        "POST",
        "/v1/hosts/ssh",
        &token,
        json!({"target":"resume-node","label":"durable bridge test"}),
    )
    .await;
    assert_eq!(status, 201, "{added}");
    let id = added["id"].as_str().unwrap();
    let remote = PathBuf::from(format!("/tmp/remuda-ssh-{id}"));
    wait_host(&hub, &token, id, |host| host["online"] == true).await;
    let (status, created) = request(hub.addr, "POST", "/v1/instances", &token,
        json!({"hostId":id,"kind":"codex","driver":"codex-appserver","prompt":"fixture completion"})).await;
    assert_eq!(status, 200, "{created}");
    let instance_id = created["instance"]["instanceId"].as_str().unwrap();
    let journal_path = format!("/v1/instances/{instance_id}/journal");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, journal) = request(hub.addr, "GET", &journal_path, &token, Value::Null).await;
            if journal["durableSeq"] == "1" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    std::fs::write(remote.join("fixture-pause-bridge"), "").unwrap();
    let pid = fixture_pid(id);
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", pid.trim()])
            .status()
            .unwrap()
            .success()
    );
    wait_host(&hub, &token, id, |host| host["state"] == "offline-alive").await;
    // This spans the Hub's one-second reaper tick and far exceeds lost grace.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        std::fs::read_to_string(remote.join("fixture-DONE")).unwrap(),
        "DONE\n"
    );
    let (_, disconnected) = request(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &token,
        Value::Null,
    )
    .await;
    assert_eq!(disconnected["lifecycle"], "running", "{disconnected}");
    assert_eq!(disconnected["connectivity"], "disconnected");
    assert_ne!(disconnected["lastError"], "host-lost");
    std::fs::remove_file(remote.join("fixture-pause-bridge")).unwrap();
    wait_host(&hub, &token, id, |host| {
        host["online"] == true && host["lastError"].is_null()
    })
    .await;
    let journal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, journal) = request(hub.addr, "GET", &journal_path, &token, Value::Null).await;
            if journal["durableSeq"] == "2" {
                break journal;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(journal["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        journal["events"][1]["event"]["payload"]["reasonCode"],
        "fixture-completed"
    );
    let watermarks: Value = serde_json::from_str(
        &std::fs::read_to_string(remote.join("fixture-watermarks.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(watermarks[0]["instanceId"], instance_id);
    assert_eq!(
        watermarks[0]["durableSeq"], "1",
        "hello snapshot must not advance Hub's acked seq"
    );
    let (_, complete) = request(
        hub.addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        &token,
        Value::Null,
    )
    .await;
    assert_eq!(complete["lifecycle"], "exited");
    assert_eq!(complete["durableSeq"], "2");
    hub.shutdown().await;
    std::fs::remove_dir_all(remote).unwrap();
}

// D-048: an SSH-attached proxy host H going down must still run the relay
// teardown — api.egress revoke + api.end toward W + blocked{api-route-down}.
// The observable Hub-side effect over HTTP is the api_route_down diagnostic
// on an instance routed through H (the ws-attached worker W answers create).
#[tokio::test]
async fn ssh_proxy_teardown_runs_api_relay_link_loss() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("node");
    executable(&fixture, include_str!("fixtures/ssh/fake-node.py"));
    let mut config = HubConfig::for_test(dir.path().join("hub"));
    config.ssh_hosts.ssh_binary = fake_ssh(dir.path(), &fixture);
    config.host_lost_grace_ms = 25;
    let hub = remuda_hub::spawn(config).await.unwrap();
    let token = login(&hub).await;

    // H: ssh-attached fake node (advertises apiRelay via the fixture).
    let (status, added) = request(
        hub.addr,
        "POST",
        "/v1/hosts/ssh",
        &token,
        json!({"target":"relay-h","label":"relay proxy"}),
    )
    .await;
    assert_eq!(status, 201, "{added}");
    let proxy_id = added["id"].as_str().unwrap().to_string();
    wait_host(&hub, &token, &proxy_id, |h| h["online"] == true).await;

    // W: ws-attached D-048 worker that answers instance.create and echoes the
    // requested route, projecting an instance routed through the ssh host H.
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await
        .unwrap();
    let mut upgrade = format!("ws://{}/v1/node", hub.addr)
        .into_client_request()
        .unwrap();
    upgrade
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut ws_node, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    ws_node
        .send(Message::Text(
            json!({"jsonrpc":"2.0","id":"h","method":"node.hello","params":{
                "hostId": remuda_protocol::HostId::new().as_id(),
                "nodeVersion":"0.2.0-d048","label":"w",
                "capabilities":{"apiRelay":true},
                "host":{"maxInstances":8,"cli":[{"kind":"claude","auth":"gateway-logged-in"}]}}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let hello: Value =
        serde_json::from_str(ws_node.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let worker_id = hello["result"]["hostId"].as_str().unwrap().to_string();

    // Gateway profile.
    let (status, profile) = request(
        hub.addr,
        "POST",
        "/v1/providers",
        &token,
        json!({"name":"g","kind":"gateway","baseUrl":"http://127.0.0.1:1/v1",
               "authToken":"sk-fake-profile-0001","defaultGateway":true}),
    )
    .await;
    assert_eq!(status, 200, "{profile}");

    // Background task: answer the single instance.create RPC, echoing route.
    let answer_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_node.next().await {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").and_then(Value::as_str) == Some("instance.create")
                && let Some(id) = frame.get("id").cloned()
            {
                let route = frame
                    .pointer("/params/spec/apiRoute")
                    .cloned()
                    .unwrap_or(Value::Null);
                let reply = json!({"jsonrpc":"2.0","id":id,
                    "result":{"accepted":true,"instanceId":frame["params"]["instanceId"],
                              "driver":"claude-print","apiRoute":route}});
                let _ = ws_node.send(Message::Text(reply.to_string().into())).await;
            }
        }
    });

    let (status, created) = request(
        hub.addr,
        "POST",
        "/v1/instances",
        &token,
        json!({
            "hostId": worker_id,
            "kind": "claude",
            "driver": "claude-print",
            "delegation": "gateway",
            "apiVia": proxy_id,
            "apiRoute": "hub-relay"
        }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_string();

    // Kill the SSH proxy process → connect_once returns → on_link_lost runs.
    let pid = fixture_pid(&proxy_id);
    std::process::Command::new("kill")
        .args(["-TERM", pid.trim()])
        .status()
        .unwrap();

    // Confirm the ssh carrier actually dropped first.
    wait_host(&hub, &token, &proxy_id, |h| h["online"] == false).await;
    // The instance journal must carry the api_route_down Hub diagnostic.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (_, journal) = request(
                hub.addr,
                "GET",
                &format!("/v1/instances/{instance_id}/journal"),
                &token,
                Value::Null,
            )
            .await;
            let has = journal
                .pointer("/events")
                .and_then(Value::as_array)
                .is_some_and(|events| {
                    events.iter().any(|e| {
                        e.pointer("/event/payload/nativeName")
                            .and_then(Value::as_str)
                            == Some("api_route_down")
                    })
                });
            if has {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("api_route_down diagnostic must be journaled when ssh proxy drops");

    answer_task.abort();
    hub.shutdown().await;
}
