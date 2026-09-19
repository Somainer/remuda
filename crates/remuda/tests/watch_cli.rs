//! `remuda watch` / `remuda worker` / `remuda report` against the in-process
//! Hub and a scripted-screen fake Node. Covers the CLI surface of every
//! classification (new DONE vs echo), the worker intervention verbs, and the
//! `--for-owner` change-driven report. Classification logic itself is covered
//! in remuda-hub/tests/watch.rs.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

#[derive(Clone)]
struct ScriptedScreen {
    supported: bool,
    lifecycle: String,
    lines: Vec<String>,
}

struct Hub {
    _dir: tempfile::TempDir,
    _hub: remuda_hub::RunningHub,
    base: String,
    token: String,
    host: String,
    workspace: String,
    screen: Arc<Mutex<ScriptedScreen>>,
    feed: tokio::sync::mpsc::UnboundedSender<Value>,
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

/// Next inbound frame worth answering, or `None` once the stream closed.
///
/// An idle timeout is retried rather than ending the fake Node: the Hub
/// sends nothing between `runtime.hello` and the first watch/dispatch RPC,
/// while the CLI subprocesses before it can sit longer than the frame
/// window on a loaded gate host. Letting the serving task exit dropped the
/// socket and the Hub marked the host offline.
async fn next_rpc_frame<S>(ws: &mut S) -> Option<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str(&text) {
                Ok(frame) => return Some(frame),
                Err(_) => continue,
            },
            Ok(Some(Ok(_))) => continue,
            Ok(None) | Ok(Some(Err(_))) => return None,
            Err(_) => continue,
        }
    }
}

async fn spawn_hub() -> Result<Hub> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("watch-cli").await?;
    let host = remuda_protocol::HostId::new().as_id().to_string();
    let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
    let screen: Arc<Mutex<ScriptedScreen>> = Arc::new(Mutex::new(ScriptedScreen {
        supported: true,
        lifecycle: "ready".to_string(),
        lines: Vec::new(),
    }));
    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

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
        while let Some(frame) = next_rpc_frame(&mut node).await {
            // Drain scripted journal events before replying: the hub awaits
            // this reply before reading the journal tail, and sends no other
            // RPC meanwhile, so the inbound frames below are the acks.
            while let Ok(feed) = feed_rx.try_recv() {
                if node
                    .send(Message::Text(feed.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                if next_rpc_frame(&mut node).await.is_none() {
                    break;
                }
            }
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
                        let state = node_screen.lock().unwrap().clone();
                        json!({
                            "instanceId": frame["params"]["instanceId"],
                            "supported": state.supported,
                            "lifecycle": state.lifecycle,
                            "driver": "claude-print",
                            "cols": 80, "rows": 24,
                            "source": "emulator", "lines": state.lines,
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
    let base = format!("http://{}", hub.addr);
    let liveness = remuda_hub_client::HubClient::new(base.clone(), Some(token.clone()), None)?;
    // Wait for the hosts index to project the live link before tests
    // dispatch, instead of racing it with a fixed sleep; the keepalive
    // serving loop above also keeps the host online through slow CLI
    // subprocess sequences on a loaded gate host.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if liveness.list_hosts().await.is_ok_and(|rows| {
                rows.iter().any(|row| {
                    row["hostId"].as_str() == Some(host.as_str()) && row["online"] == true
                })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok()
    .context("fake host never reported online before dispatch")?;
    Ok(Hub {
        _dir: dir,
        _hub: hub,
        base,
        token,
        host,
        workspace,
        screen,
        feed: feed_tx,
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
        *self.screen.lock().unwrap() = ScriptedScreen {
            supported: true,
            lifecycle: "ready".to_string(),
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
        };
    }

    /// No readable screen (claude-print / dead carrier): observations must
    /// classify from the scripted journal.
    fn set_no_screen(&self, lifecycle: &str) {
        *self.screen.lock().unwrap() = ScriptedScreen {
            supported: false,
            lifecycle: lifecycle.to_string(),
            lines: Vec::new(),
        };
    }

    /// Queue journal events for the worker's instance; flushed on the next
    /// hub→node RPC (the `watch` screen read).
    fn append_journal(&self, instance_id: &str, events: &[Value]) {
        for (seq, event) in events.iter().enumerate() {
            self.feed
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": format!("feed-{seq}"),
                    "method": "journal.append",
                    "params": { "instanceId": instance_id, "event": event },
                }))
                .unwrap();
        }
    }

    async fn worker_instance_id(&self, project_id: &str, name: &str) -> Result<String> {
        let client =
            remuda_hub_client::HubClient::new(self.base.clone(), Some(self.token.clone()), None)?;
        let roster = client
            .get(&format!("/v1/workers?project={project_id}"))
            .await?;
        roster["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == json!(name))
            .and_then(|row| row["instanceId"].as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("no instanceId for {name}"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screenless_failed_first_turn_surfaces_in_watch_and_report() -> Result<()> {
    let hub = spawn_hub().await?;
    let range = "58960-58969";
    let project = hub.setup_project(range).await?;
    let name = format!("c-fail-{}", std::process::id());
    hub.dispatch(&project, &name, range).await;
    hub.set_no_screen("ready");
    let instance = hub.worker_instance_id(&project, &name).await?;
    hub.append_journal(
        &instance,
        &[
            json!({
                "kind": "message",
                "payload": {
                    "role": "assistant", "phase": "final",
                    "blocks": [{ "type": "text",
                                 "text": "API Error: 400 requested model is not available" }],
                },
            }),
            json!({
                "kind": "lifecycle",
                "payload": {
                    "type": "native", "topic": "turn", "nativeName": "result",
                    "status": { "value": "error" }, "affectsCompletion": true,
                },
            }),
        ],
    );

    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout)?;
    let row = &body["items"][0];
    assert_eq!(row["watch"]["status"], "failed", "{body}");
    assert_eq!(
        row["watch"]["reason"],
        "API Error: 400 requested model is not available"
    );
    assert!(
        row["watch"]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("screen-unavailable"),
        "{}",
        row["watch"]["detail"]
    );

    // The compact table carries the same reason and provenance.
    let output = hub.run(&["watch", "--once"]);
    assert!(output.status.success());
    let table = String::from_utf8_lossy(&output.stdout);
    assert!(table.contains("failed"), "{table}");
    assert!(table.contains("API Error: 400"), "{table}");
    assert!(table.contains("screen-unavailable"), "{table}");

    // Full digest lists it under needs attention.
    let output = hub.run(&["report", "--json"]);
    assert!(output.status.success());
    let digest: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        digest["needsAttention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|worker| worker["name"] == json!(name) && worker["watch"]["status"] == "failed"),
        "{digest}"
    );
    let text = hub.run(&["report"]);
    assert!(text.status.success());
    let rendered = String::from_utf8_lossy(&text.stdout);
    assert!(rendered.contains("(failed)"), "{rendered}");
    assert!(rendered.contains("screen-unavailable"), "{rendered}");
    // The reason must not be printed twice on the attention line.
    let doubled = "API Error: 400 requested model is not available screen-unavailable; API Error";
    assert!(!rendered.contains(doubled), "{rendered}");

    // Owner report includes the failed worker exactly once.
    let output = hub.run(&["report", "--for-owner", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let asks: Value = serde_json::from_slice(&output.stdout)?;
    let mine: Vec<_> = asks["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|ask| ask["name"] == json!(name))
        .collect();
    assert_eq!(mine.len(), 1, "failed worker appears once: {asks}");
    assert_eq!(mine[0]["kind"], "failed");
    assert_eq!(
        mine[0]["reason"],
        "API Error: 400 requested model is not available"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screenless_idle_after_api_error_is_nudge_not_failure() -> Result<()> {
    let hub = spawn_hub().await?;
    let range = "58970-58979";
    let project = hub.setup_project(range).await?;
    let name = format!("c-idle-{}", std::process::id());
    hub.dispatch(&project, &name, range).await;
    hub.set_no_screen("ready");
    let instance = hub.worker_instance_id(&project, &name).await?;
    hub.append_journal(
        &instance,
        &[json!({
            "kind": "message",
            "payload": {
                "role": "assistant", "phase": "final",
                "blocks": [{ "type": "text",
                             "text": "API Error: bad gateway, Retrying (2/5)…" }],
            },
        })],
    );

    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["items"][0]["watch"]["status"], "idle-api-error");
    assert!(
        body["items"][0]["watch"]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("screen-unavailable")
    );

    // Idle-after-error needs a nudge, not an owner ask.
    let output = hub.run(&["report", "--for-owner", "--json"]);
    assert!(output.status.success());
    let asks: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        asks["items"]
            .as_array()
            .is_none_or(|items| items.iter().all(|ask| ask["name"] != json!(name))),
        "{asks}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screenless_done_from_journal_then_echo() -> Result<()> {
    let hub = spawn_hub().await?;
    let range = "58980-58989";
    let project = hub.setup_project(range).await?;
    hub.dispatch(&project, "c-donejs", range).await;
    hub.set_no_screen("ready");
    let instance = hub.worker_instance_id(&project, "c-donejs").await?;
    hub.append_journal(
        &instance,
        &[json!({
            "kind": "message",
            "payload": {
                "role": "assistant", "phase": "final",
                "blocks": [{ "type": "text", "text": "DONE 0123abcd4567" }],
            },
        })],
    );

    // First observation: fresh DONE from the journal.
    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["items"][0]["watch"]["status"], "done");
    assert_eq!(body["items"][0]["watch"]["sha"], "0123abcd4567");
    assert_eq!(body["items"][0]["state"]["state"], "done");

    // Second observation: same journaled tip is an echo — working, claim kept.
    let output = hub.run(&["watch", "--once", "--json"]);
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(body["items"][0]["watch"]["status"], "working", "{body}");
    assert_eq!(body["items"][0]["state"]["state"], "done");
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
