//! `remuda watch` / `remuda worker` / `remuda report` against the in-process
//! Hub and a scripted-screen fake Node. Covers the CLI surface of every
//! classification (new DONE vs echo), the worker intervention verbs, and the
//! `--for-owner` change-driven report. Classification logic itself is covered
//! in remuda-hub/tests/watch.rs.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
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
    host: String,
    workspace: String,
    screen: Arc<Mutex<Vec<String>>>,
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
    let token = hub.mint_device_token("watch-cli").await?;
    let host = remuda_protocol::HostId::new().as_id().to_string();
    let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
    let screen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

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

    let node_screen = screen.clone();
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
                    "tty.screen" => {
                        let lines = node_screen.lock().unwrap().clone();
                        json!({
                            "instanceId": frame["params"]["instanceId"],
                            "supported": true, "lifecycle": "ready", "driver": "claude-pty",
                            "cols": 80, "rows": 24, "source": "emulator", "lines": lines,
                        })
                    }
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
        screen,
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

    fn set_screen(&self, lines: &[&str]) {
        *self.screen.lock().unwrap() = lines.iter().map(|line| (*line).to_string()).collect();
    }

    async fn setup_project(&self, range: &str) -> Result<String> {
        let client =
            remuda_hub_client::HubClient::new(self.base.clone(), Some(self.token.clone()), None)?;
        let created = client
            .post("/v1/projects", &json!({ "name": "cli-watch" }))
            .await?;
        let project_id = created["id"].as_str().unwrap().to_string();
        client
            .post(
                &format!("/v1/projects/{project_id}/members"),
                &json!({"hostId": self.host, "workspaceId": self.workspace, "role": "build"}),
            )
            .await?;
        client
            .patch(
                &format!("/v1/projects/{project_id}"),
                &json!({
                    "hosts": [{
                        "hostId": self.host, "maxInstances": 8, "maxBuilding": 4,
                        "diskBudgetGb": 10, "portBlocks": [range],
                        "requires": ["toolchain=rust"], "latencyClass": "remote",
                    }],
                }),
            )
            .await?;
        Ok(project_id)
    }

    async fn dispatch(&self, project_id: &str, name: &str, range: &str) {
        let brief = self._dir.path().join("brief.md");
        std::fs::write(
            &brief,
            "Do the trivial task.\nReply on one line: DONE <sha> or BLOCKED <reason>.\n",
        )
        .unwrap();
        let output = self.run(&[
            "dispatch",
            "--project",
            project_id,
            "--brief",
            brief.to_str().unwrap(),
            "--name",
            name,
        ]);
        assert!(
            output.status.success(),
            "dispatch {}: {}",
            range,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_once_json_classifies_done_then_echo() -> Result<()> {
    let hub = spawn_hub().await?;
    let project = hub.setup_project("58600-58629").await?;
    hub.dispatch(&project, "c-watch", "58600-58629").await;

    hub.set_screen(&["working…", "DONE 0a10ebf351aa"]);
    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["items"][0]["watch"]["status"], "done");
    assert_eq!(body["items"][0]["state"]["state"], "done");

    // Same DONE re-shown is an echo: watch says working, claim stays done.
    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["items"][0]["watch"]["status"], "working");
    assert_eq!(body["items"][0]["state"]["state"], "done");

    // The compact table renders too.
    hub.set_screen(&["• BLOCKED need a token please"]);
    let output = hub.run(&["watch"]);
    assert!(output.status.success());
    let table = String::from_utf8_lossy(&output.stdout);
    assert!(table.contains("c-watch"), "{table}");
    assert!(table.contains("blocked"), "{table}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_follow_exits_zero_once_done() -> Result<()> {
    let hub = spawn_hub().await?;
    let project = hub.setup_project("58630-58659").await?;
    hub.dispatch(&project, "c-follow", "58630-58659").await;
    hub.set_screen(&["DONE 1111aaaa2222"]);
    let output = hub.run(&["watch", "--follow", "--json", "--interval-secs", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"allDone\":true"), "{stdout}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_verbs_go_through_hub() -> Result<()> {
    let hub = spawn_hub().await?;
    let project = hub.setup_project("58660-58689").await?;
    hub.dispatch(&project, "c-verb", "58660-58689").await;

    // answer enter → tty.write CR.
    let output = hub.run(&["worker", "answer", "c-verb", "enter"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["keys"], json!(["enter"]));

    // answer a digit.
    let output = hub.run(&["worker", "answer", "c-verb", "3"]);
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["keys"], json!(["3"]));

    // nudge succeeds and is throttled on the immediate repeat.
    let output = hub.run(&["worker", "nudge", "c-verb"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = hub.run(&["worker", "nudge", "c-verb"]);
    assert!(!output.status.success(), "second nudge must be throttled");

    // stop closes but does not reclaim.
    let output = hub.run(&["worker", "stop", "c-verb"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_for_owner_emits_only_on_change() -> Result<()> {
    let hub = spawn_hub().await?;
    let range = "58690-58719";
    let project = hub.setup_project(range).await?;
    // Unique name + reason so the persisted change-marker never suppresses a
    // re-run of this test in the same worktree.
    let name = format!("c-owner-{}", std::process::id());
    hub.dispatch(&project, &name, range).await;
    let reason = format!("need owner credential {}", std::process::id());
    hub.set_screen(&[&format!("BLOCKED {reason}")]);
    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(output.status.success());

    let output = hub.run(&["report", "--for-owner", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout)?;
    let items = body["items"].as_array().expect("items");
    assert!(
        items.iter().any(|ask| ask["name"] == json!(name)
            && ask["kind"] == "blocked"
            && ask["reason"] == json!(reason)),
        "owner report should carry the BLOCKED ask: {body}"
    );

    // The idle-after-API-error state is coordinator-actionable, NOT
    // owner-actionable: switch the screen to that and confirm no new ask.
    hub.set_screen(&["API Error: bad gateway", "Retrying…"]);
    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(output.status.success());
    let output = hub.run(&["report", "--for-owner", "--json"]);
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        body["items"]
            .as_array()
            .is_none_or(|items| items.iter().all(|ask| ask["name"] != json!(name))),
        "an idle-after-API-error worker needs a nudge, not an owner ask: {body}"
    );
    Ok(())
}

#[test]
fn help_lists_watch_worker_report() -> Result<()> {
    for (command, pieces) in [
        ("watch", &["--once", "--follow", "--json", "--project"][..]),
        (
            "worker",
            &[
                "nudge",
                "answer",
                "switch-model",
                "resume",
                "replace",
                "stop",
            ][..],
        ),
        ("report", &["--for-owner", "--json", "--project"][..]),
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
