//! Real Node runtime over a fake local SSH executable, including upload and dispatch.
use remuda_hub::{HubConfig, RunningHub};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("host status deadline")
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

struct FixtureDaemon(PathBuf);

impl Drop for FixtureDaemon {
    fn drop(&mut self) {
        let Ok(pid) = std::fs::read_to_string(self.0.join("node.pid")) else {
            return;
        };
        let Ok(pid) = pid.trim().parse::<u32>() else {
            return;
        };
        // The unique fixture directory must still identify this exact process;
        // never signal an unrelated process if a stale pidfile was left behind.
        let Ok(command) = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
        else {
            return;
        };
        if !String::from_utf8_lossy(&command.stdout).contains(self.0.to_string_lossy().as_ref()) {
            return;
        }
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        for _ in 0..100 {
            if !self.0.join("node.pid").exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_stdio_node_upload_dispatch_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let node = Path::new(env!("CARGO_BIN_EXE_remuda-node-stdio"));
    let fake = remuda_testing::ensure_workspace_bin("fake-claude");
    let script = remuda_testing::script_path(remuda_testing::ScriptKind::Ok);
    let ssh = fake_ssh(dir.path(), node);
    let user_codex = dir.path().join("home/.codex");
    std::fs::create_dir_all(&user_codex).unwrap();
    std::fs::write(user_codex.join("auth.json"), "fixture-user-auth").unwrap();
    let original = std::fs::read_to_string(&ssh).unwrap();
    executable(
        &ssh,
        &original.replace(
            "for arg do",
            &format!(
                "export REMUDA_CLAUDE_BIN='{}'\nexport FAKE_CLAUDE_SCRIPT='{}'\nfor arg do",
                fake.display(),
                script.display()
            ),
        ),
    );
    let mut config = HubConfig::for_test(dir.path().join("hub"));
    config.command_accept_timeout_ms = 20000;
    config.ssh_hosts.ssh_binary = ssh;
    config.ssh_hosts.upload_binary = Some(node.into());
    // Exercise an absent remote binary: only the uploaded copy can start the Node.
    std::fs::remove_file(dir.path().join("bin/remuda")).unwrap();
    let hub = remuda_hub::spawn(config).await.unwrap();
    let token = login(&hub).await;
    let (status, added) = request(hub.addr,"POST","/v1/hosts/ssh",&token,json!({"target":"local-test-node","label":"Native stdio fixture","labels":["egress:gateway"],"remuda_binary_policy":"upload_if_missing"})).await;
    assert_eq!(status, 201, "{added}");
    let id = added["id"].as_str().unwrap();
    let remote = PathBuf::from(format!("/tmp/remuda-ssh-{id}"));
    let daemon = FixtureDaemon(remote.clone());
    wait_host(&hub, &token, id, |host| host["online"] == true).await;
    assert!(remote.join("remuda").is_file());
    assert!(!remote.join("codex/auth.json").exists());
    let (status, created)=request(hub.addr,"POST","/v1/instances",&token,json!({"hostId":id,"kind":"claude","driver":"claude-print","delegation":"none","prompt":"Reply OK","model":"fake"})).await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["hostId"], id);
    assert_eq!(created["command"]["state"], "accepted", "{created}");
    let instance = created["instance"]["instanceId"].as_str().unwrap();
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
        409
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (_, journal) = request(
                hub.addr,
                "GET",
                &format!("/v1/instances/{instance}/journal"),
                &token,
                Value::Null,
            )
            .await;
            if journal.to_string().contains("\"OK\"") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("actual Node journal mirrored");
    let (status, closed) = request(
        hub.addr,
        "POST",
        &format!("/v1/instances/{instance}/commands"),
        &token,
        json!({"operation":"instance.close","payload":{}}),
    )
    .await;
    assert_eq!(status, 200, "{closed}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, _) = request(
                hub.addr,
                "DELETE",
                &format!("/v1/hosts/{id}"),
                &token,
                Value::Null,
            )
            .await;
            if status == 204 {
                break;
            }
            assert_eq!(status, 409);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("close settles before remove");
    hub.shutdown().await;
    assert!(
        remuda_node::daemon_is_running(&remote).await.unwrap(),
        "Hub shutdown must leave the persistent Node alive"
    );
    drop(daemon);
    assert!(
        !remote.join("node.pid").exists(),
        "fixture daemon shutdown must complete before cleanup"
    );
    std::fs::remove_dir_all(remote).unwrap();
}
