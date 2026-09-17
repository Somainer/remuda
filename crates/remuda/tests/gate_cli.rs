//! `remuda gate` / `remuda land` / `remuda gate list|cancel` against the
//! in-process Hub and a scripted lane Node that answers `gate.run`, streams
//! `gate.event` steps, and serves the `gate.then` post-land hook.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

struct Hub {
    _dir: tempfile::TempDir,
    _hub: remuda_hub::RunningHub,
    base: String,
    token: String,
    project: String,
    _node: tokio::task::JoinHandle<()>,
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected frame {other:?}")),
    }
}

async fn spawn_hub() -> Result<Hub> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("gate-cli").await?;
    let host = remuda_protocol::HostId::new().as_id().to_string();
    let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();

    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host, "nodeVersion": "0.1.0-test",
                "label": "fake-lane-host",
                "host": {
                    "hostname": "fake-lane-host", "maxInstances": 8,
                    "labels": {}, "resources": {"cpuCount": 8},
                    "workspaces": [{"workspaceId": workspace, "hostId": host, "root": "/tmp/repo"}],
                    "workspaceRevision": 1,
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    assert!(hello["result"]["nodeToken"].is_string());

    let task = tokio::spawn(async move {
        let mut seq = 0u64;
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            match method {
                "gate.run" => {
                    let job = params["jobId"].as_str().unwrap_or("").to_string();
                    let mode = params["mode"].as_str().unwrap_or("verify");
                    // Stream two step notifications.
                    for step in [("secret-scan", "ok", 11u64), ("cargo-test", "ok", 22)] {
                        seq += 1;
                        let event = json!({
                            "jsonrpc": "2.0", "id": format!("evt-{seq}"),
                            "method": "gate.event",
                            "params": {"jobId": job, "kind": "step",
                                "step": {"name": step.0, "status": step.1,
                                          "durationMs": step.2, "attempts": 1}},
                        });
                        if node
                            .send(Message::Text(event.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    let (status, sha) = if mode == "land" {
                        ("landed", "5555555555555555555555555555555555555555")
                    } else {
                        ("passed", "3333333333333333333333333333333333333333")
                    };
                    let result = json!({
                        "jobId": job, "status": status,
                        "mergeSha": sha,
                        "baseSha": "1111111111111111111111111111111111111111",
                        "headSha": "2222222222222222222222222222222222222222",
                        "steps": [
                            {"name": "secret-scan", "status": "ok", "durationMs": 11},
                            {"name": "cargo-test", "status": "ok", "durationMs": 22}
                        ],
                    });
                    let reply = json!({"jsonrpc":"2.0","id":id,"result":result});
                    if node
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                "gate.then" => {
                    let job = params["jobId"].as_str().unwrap_or("").to_string();
                    let command = params["command"].as_str().unwrap_or("");
                    assert!(command.contains("demo-refresh"), "{command}");
                    let result = json!({
                        "jobId": job, "exitCode": 0,
                        "output": "demo refreshed\n",
                    });
                    let reply = json!({"jsonrpc":"2.0","id":id,"result":result});
                    if node
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ => {
                    let reply = json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}});
                    let _ = node.send(Message::Text(reply.to_string().into())).await;
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let base = format!("http://{}", hub.addr);
    // Configure the project with one gate lane on the enrolled host.
    let client = remuda_hub_client::HubClient::new(base.clone(), Some(token.clone()), None)?;
    let created = client
        .post(
            "/v1/projects",
            &json!({
                "name": "cli-gate",
                "members": [{"hostId": host, "workspaceId": workspace, "role": "build"}],
                "hosts": [{"hostId": host, "maxInstances": 8}],
                "homeHost": host,
                "gate": {
                    "affected": true, "web": "auto",
                    "lanes": [{
                        "id": "lane1", "hostId": host,
                        "repoPath": "/tmp/lane/repo", "targetDir": "/tmp/lane/target",
                        "ports": "58480-58489",
                    }],
                },
            }),
        )
        .await?;
    let project = created["id"].as_str().unwrap().to_string();

    Ok(Hub {
        _dir: dir,
        _hub: hub,
        base,
        token,
        project,
        _node: task,
    })
}

impl Hub {
    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut all = args.to_vec();
        all.extend(["--hub", &self.base, "--token", &self.token]);
        std::process::Command::new(bin())
            .args(all)
            .output()
            .expect("run remuda")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_verify_prints_live_steps_and_exits_zero() -> Result<()> {
    let hub = spawn_hub().await?;
    let output = hub.run(&["gate", "wt/cli/verify", "--project", &hub.project]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("cargo-test: ok (22 ms)"), "{stdout}");
    assert!(stdout.contains("verify: passed"), "{stdout}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_verify_json_shape_matches_merge_report_steps() -> Result<()> {
    let hub = spawn_hub().await?;
    let output = hub.run(&["gate", "wt/cli/json", "--project", &hub.project, "--json"]);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["state"], "passed");
    assert_eq!(value["steps"][0]["name"], "secret-scan");
    assert_eq!(value["steps"][0]["durationMs"], 11);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn land_waits_and_prints_the_merge_sha() -> Result<()> {
    let hub = spawn_hub().await?;
    let output = hub.run(&[
        "land",
        "wt/cli/land",
        "--project",
        &hub.project,
        "--then",
        "echo demo-refresh",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("landed: 555555555555"), "{stdout}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_list_shows_queued_and_finished_jobs() -> Result<()> {
    let hub = spawn_hub().await?;
    let _ = hub.run(&["gate", "wt/cli/list", "--project", &hub.project]);
    let output = hub.run(&["gate", "list", "--project", &hub.project]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("wt/cli/list"), "{stdout}");
    assert!(stdout.contains("passed"), "{stdout}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_help_lists_list_and_cancel() -> Result<()> {
    let hub = spawn_hub().await?;
    let output = hub.run(&["gate", "--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("list"), "{stdout}");
    assert!(stdout.contains("cancel"), "{stdout}");
    let output = hub.run(&["land", "--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--then"), "{stdout}");
    Ok(())
}
