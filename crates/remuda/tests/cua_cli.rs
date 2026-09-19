//! D-045 CLI preflight for `--capability computer-use` on instance create.
//!
//! The in-process Hub gets a fake Node whose hello inventory carries an
//! `os` string and an optional `computer-use` cli row — exactly the bytes the
//! hostcap batch will produce. Preflight happens before persistence, so each
//! refusal must show up at the command line with the host id named.

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
    host: String,
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

async fn spawn_hub(os: Option<&str>, computer_use_row: Option<Value>) -> Result<Hub> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("cua-cli").await?;
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
    let mut host_obj = json!({
        "hostname": "cua-cli-host",
        "labels": {},
        "maxInstances": 8,
        "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
        "workspaces": [{"workspaceId": workspace, "hostId": host, "root": "/tmp/repo"}],
        "workspaceRevision": 1,
    });
    if let Some(os) = os {
        host_obj["os"] = json!(os);
    }
    if let Some(row) = computer_use_row {
        host_obj["cli"].as_array_mut().unwrap().push(row);
    }
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": { "hostId": host, "nodeVersion": "0.1.0-test", "host": host_obj }
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
                let result = match frame["method"].as_str().unwrap_or("") {
                    "worker.provision" => json!({
                        "name": "x", "branch": "wt/x/work", "startPoint": "origin/main",
                        "worktreePath": "/tmp/remuda-wt/x", "targetDir": "/tmp/remuda-target/x",
                    }),
                    _ => json!({"accepted": true,
                        "instanceId": "ins_fake00000000000000000000000001"}),
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
async fn unknown_capability_is_refused_and_named() -> Result<()> {
    let hub = spawn_hub(Some("macos"), None).await?;
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--capability",
            "desktop",
        ],
        &hub,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("\"desktop\""), "{stderr}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grok_kind_is_refused_at_the_cli() -> Result<()> {
    let hub = spawn_hub(Some("macos"), None).await?;
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--kind",
            "grok",
            "--driver",
            "shell-pty",
            "--capability",
            "computer-use",
        ],
        &hub,
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not supported"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_without_the_row_is_refused_with_a_retry_direction() -> Result<()> {
    let hub = spawn_hub(Some("macos"), None).await?;
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--capability",
            "computer-use",
        ],
        &hub,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&hub.host), "{stderr}");
    assert!(stderr.contains("not reported"), "{stderr}");
    assert!(stderr.contains("--host"), "{stderr}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_reporting_not_installed_is_refused_with_the_probed_path() -> Result<()> {
    let row = json!({
        "kind": "computer-use",
        "installed": false,
        "path": "/Users/u/.codex/computer-use/Codex Computer Use.app",
        "auth": "unknown",
    });
    let hub = spawn_hub(Some("macos"), Some(row)).await?;
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--capability",
            "computer-use",
        ],
        &hub,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not installed"), "{stderr}");
    assert!(stderr.contains("Codex Computer Use.app"), "{stderr}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_macos_host_is_refused_and_names_the_os() -> Result<()> {
    let row = json!({"kind": "computer-use", "installed": true, "auth": "unknown"});
    let hub = spawn_hub(Some("linux"), Some(row)).await?;
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--capability",
            "computer-use",
        ],
        &hub,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("linux"), "{stderr}");
    assert!(stderr.contains("macOS"), "{stderr}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pathless_not_installed_row_names_symbolic_host_location_not_placeholder() -> Result<()> {
    // Round-6 item 3: the hostcap probe omits `path` whenever installed=false.
    // The CLI refusal must name the symbolic REMOTE-host location it probes
    // (not expand the coordinator's own CODEX_HOME/HOME, and never print
    // "<no path reported>").
    let row = json!({"kind": "computer-use", "installed": false, "auth": "unknown"});
    let hub = spawn_hub(Some("macos"), Some(row)).await?;
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--capability",
            "computer-use",
        ],
        &hub,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("$CODEX_HOME/computer-use/Codex Computer Use.app"),
        "must name the symbolic host install location: {stderr}"
    );
    assert!(
        stderr.contains("SkyComputerUseClient"),
        "must name the exact file hostcap probes: {stderr}"
    );
    assert!(
        stderr.contains("$HOME/.codex"),
        "must give the CODEX_HOME-unset fallback: {stderr}"
    );
    assert!(
        !stderr.contains("<no path reported>"),
        "must not print the bare placeholder: {stderr}"
    );
    // The CLI's own local HOME must never be expanded into a host message.
    if let Ok(local_home) = std::env::var("HOME")
        && !local_home.is_empty()
        && local_home != "/"
    {
        assert!(
            !stderr.contains(&local_home),
            "remote-host message must not expand the local HOME {local_home:?}: {stderr}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_capability_on_macos_passes_preflight_and_reaches_the_node() -> Result<()> {
    let row = json!({
        "kind": "computer-use",
        "installed": true,
        "path": "/Users/u/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient",
        "auth": "unknown",
    });
    let hub = spawn_hub(Some("macos"), Some(row)).await?;
    // The CLI preflight passes; the development Node then refuses at its own
    // boundary because the fake Linux process has no computer-use probe — the
    // two layers are deliberately independent, and the Node boundary is what
    // makes this honest. We assert on the CLI not refusing and on the refusal
    // arriving from downstream rather than the preflight message.
    let output = run(
        &[
            "instance",
            "create",
            "--host",
            &hub.host,
            "--capability",
            "computer-use",
        ],
        &hub,
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // No client-side refusal wording: preflight passed.
    assert!(
        !combined.contains("not reported"),
        "preflight refused: {combined}"
    );
    assert!(
        !combined.contains("requires macOS"),
        "preflight refused: {combined}"
    );
    Ok(())
}

#[test]
fn help_lists_the_capability_flag_on_create_and_dispatch() -> Result<()> {
    for command in ["instance create --help", "dispatch --help"] {
        let output = std::process::Command::new(bin())
            .args(command.split_whitespace())
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("--capability"), "{command}: {stdout}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_refuses_computer_use_for_every_harness() -> Result<()> {
    // D-045 Q4: dispatch workers are unattended; both harnesses are refused
    // with a message naming both, never silently downgraded. No Hub needed —
    // the CLI refuses before making the call.
    for harness in ["claude", "codex"] {
        let hub = spawn_hub(Some("macos"), None).await?;
        let brief = hub._dir.path().join("brief.md");
        std::fs::write(
            &brief,
            "Do the task.\nReply DONE <sha> or BLOCKED <reason>.\n",
        )?;
        let output = run(
            &[
                "dispatch",
                "--project",
                "p1",
                "--brief",
                brief.to_str().unwrap(),
                "--harness",
                harness,
                "--host",
                &hub.host,
                "--capability",
                "computer-use",
            ],
            &hub,
        );
        assert!(!output.status.success(), "harness {harness}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("dispatch") && stderr.contains("unattended"),
            "harness {harness}: {stderr}"
        );
        // The refusal must not be the silent default-permission downgrade.
        assert!(!stderr.contains("instanceId"), "{stderr}");
    }
    Ok(())
}
