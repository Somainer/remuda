//! `remuda dispatch` / `remuda retire` / `remuda hostcap` / `remuda brief`
//! against the in-process Hub and a fake Node WebSocket that answers
//! `worker.provision` / `worker.remove`. No real git or herdr: the Node side is
//! covered by remuda-node's own worker tests on a temp repo.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

// ── brief lint (no Hub needed) ─────────────────────────────────────────────

#[test]
fn brief_lint_exit_codes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let clean = dir.path().join("clean.md");
    std::fs::write(
        &clean,
        "Do the work in $WORKTREE.\nReply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", clean.to_str().unwrap()])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let backticks = dir.path().join("backticks.md");
    std::fs::write(
        &backticks,
        "Run `cargo test` then reply DONE <sha> or BLOCKED <reason>.\n",
    )?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", backticks.to_str().unwrap()])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("backtick"), "{stderr}");

    let no_contract = dir.path().join("no-contract.md");
    std::fs::write(&no_contract, "Just fix the bug.\n")?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", no_contract.to_str().unwrap()])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing-contract"), "{stderr}");

    // JSON mode emits the report on stdout even on failure.
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", no_contract.to_str().unwrap(), "--json"])
        .output()?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["ok"], false);
    assert!(
        report["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["rule"] == "missing-contract")
    );
    Ok(())
}

#[test]
fn help_lists_new_verbs() -> Result<()> {
    for (command, pieces) in [
        (
            "dispatch",
            &["--project", "--brief", "--harness", "--host"][..],
        ),
        ("retire", &["--force"][..]),
        ("hostcap", &[] as &[&str]),
        ("brief", &["lint", "send"][..]),
    ] {
        let output = std::process::Command::new(bin())
            .args([command, "--help"])
            .output()?;
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        for piece in pieces {
            assert!(stdout.contains(piece), "`{command} --help` missing {piece}");
        }
    }
    Ok(())
}

// ── dispatch → roster → retire against a fake node ─────────────────────────

struct Hub {
    _dir: tempfile::TempDir,
    _hub: remuda_hub::RunningHub,
    base: String,
    token: String,
    host: String,
    workspace: String,
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
    let token = hub.mint_device_token("dispatch-cli").await?;
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
                "hostId": host,
                "nodeVersion": "0.1.0-test",
                "host": {
                    "hostname": "fake-cli-host",
                    "labels": {"toolchain": "rust"},
                    "maxInstances": 8,
                    "resources": {"cpuCount": 8, "cpuPct": 3, "memPct": 10, "diskFreeGb": 200.0},
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr": {"version": "0.9.0", "socket": "/tmp/fake.sock"},
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
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if let Some(id) = frame.get("id").cloned()
                && frame.get("method").is_some()
            {
                let method = frame["method"].as_str().unwrap_or("");
                let result = match method {
                    "worker.provision" => {
                        let name = frame["params"]["name"].as_str().unwrap_or("x");
                        let branch = frame["params"]["branch"].as_str().unwrap_or("wt/x/work");
                        json!({
                            "name": name, "branch": branch, "startPoint": "origin/main",
                            "worktreePath": format!("/tmp/remuda-wt/{name}"),
                            "targetDir": format!("/tmp/remuda-target/{name}"),
                        })
                    }
                    "worker.remove" => json!({
                        "name": frame["params"]["name"].as_str().unwrap_or("x"),
                        "worktreeRemoved": true, "targetRemoved": true,
                        "reclaimedBytes": "2048",
                    }),
                    _ => {
                        json!({"accepted": true, "instanceId": "ins_fake00000000000000000000000001"})
                    }
                };
                let response = json!({"jsonrpc":"2.0","id":id,"result":result});
                if node
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let base = format!("http://{}", hub.addr);
    Ok(Hub {
        _dir: dir,
        _hub: hub,
        base,
        token,
        host,
        workspace,
        _node: task,
    })
}

fn run(args: &[&str], hub: &Hub) -> std::process::Output {
    let mut all = args.to_vec();
    all.extend(["--hub", &hub.base, "--token", &hub.token]);
    std::process::Command::new(bin())
        .args(all)
        .output()
        .expect("run remuda")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_retire_hostcap_cli_lifecycle() -> Result<()> {
    let hub = spawn_hub().await?;

    // Project with a member workspace + port block range.
    let output = run(&["project", "create", "--name", "cli-dispatch"], &hub);
    assert!(
        output.status.success(),
        "project create: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let project: Value = serde_json::from_slice(&output.stdout)?;
    let project_id = project["id"].as_str().unwrap().to_string();

    // Add the member workspace and host quota through the Hub API (member
    // management also has a CLI verb; the per-host quota is JSON-only).
    let client =
        remuda_hub_client::HubClient::new(hub.base.clone(), Some(hub.token.clone()), None)?;
    client
        .post(
            &format!("/v1/projects/{project_id}/members"),
            &json!({"hostId": hub.host, "workspaceId": hub.workspace, "role": "build"}),
        )
        .await?;
    let patched = client
        .patch(
            &format!("/v1/projects/{project_id}"),
            &json!({
                "hosts": [{
                    "hostId": hub.host, "maxInstances": 8, "maxBuilding": 4,
                    "diskBudgetGb": 10, "portBlocks": ["58600-58629"],
                    "requires": ["toolchain=rust"], "latencyClass": "remote",
                }],
            }),
        )
        .await?;
    assert_eq!(patched["hosts"][0]["portBlocks"][0], "58600-58629");

    // Brief file.
    let brief = hub._dir.path().join("worker-brief.md");
    std::fs::write(
        &brief,
        "Complete the trivial task in your worktree.\n\
         Rules: never run deploy/ scripts or probe tunnels.\n\
         Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;

    let output = run(
        &[
            "dispatch",
            "--project",
            &project_id,
            "--brief",
            brief.to_str().unwrap(),
            "--name",
            "c-cli",
            "--harness",
            "claude",
        ],
        &hub,
    );
    assert!(
        output.status.success(),
        "dispatch: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout)?;
    let worker = &body["worker"];
    assert_eq!(worker["name"], "c-cli");
    assert_eq!(worker["branch"], "wt/c-cli/worker-brief-md");
    assert_eq!(worker["portBlock"], "58600-58609");
    assert_eq!(worker["state"]["state"], "working");
    assert!(worker["briefObjectId"].is_string());

    // hostcap.
    let output = run(&["hostcap", &hub.host], &hub);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cap: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(cap["cores"], 8);
    assert_eq!(cap["diskFreeGb"], 200.0);
    assert_eq!(cap["activeWorkers"], 1);
    assert_eq!(cap["portBlocksInUse"][0]["block"], "58600-58609");

    // Retire a still-working worker without --force exits non-zero.
    let output = run(&["retire", "c-cli"], &hub);
    assert!(!output.status.success());

    // --force retires it.
    let output = run(&["retire", "c-cli", "--force"], &hub);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["worker"]["state"]["state"], "retired");
    assert_eq!(body["node"]["reclaimedBytes"], "2048");
    Ok(())
}
