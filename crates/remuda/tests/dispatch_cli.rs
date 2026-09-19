//! `remuda dispatch` / `remuda retire` / `remuda hostcap` / `remuda brief`
//! against the in-process Hub and a fake Node WebSocket that answers
//! `worker.provision` / `worker.remove`. No real git or herdr: the Node side is
//! covered by remuda-node's own worker tests on a temp repo.

use anyhow::{Context, Result};
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

// ── desktop control lint (D-045 / D-046) ───────────────────────────────────

/// A brief that grants desktop control must carry the host and the bundle ids,
/// and must not also ask for a bypassed permission posture.
#[test]
fn brief_lint_desktop_control_rule() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Vocabulary, no capability: rejected, and the message names the value.
    let ungranted = dir.path().join("cua-ungranted.md");
    std::fs::write(
        &ungranted,
        "Use computer-use to read the note in TextEdit.\n\
         Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", ungranted.to_str().unwrap()])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing-capability"), "{stderr}");
    assert!(stderr.contains("computer-use"), "{stderr}");

    // The full grant: capability, host, bundle id — clean.
    let granted = dir.path().join("cua-granted.md");
    std::fs::write(
        &granted,
        "Read the note in TextEdit using computer-use.\n\
         capability: computer-use\n\
         host: hst_mac-mini\n\
         Approved apps: com.apple.TextEdit only.\n\
         Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", granted.to_str().unwrap()])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The refused combination (D-045 §4 / Q4).
    let bypassed = dir.path().join("cua-bypass.md");
    std::fs::write(
        &bypassed,
        "Read the note in TextEdit using computer-use.\n\
         capability: computer-use\n\
         host: hst_mac-mini\n\
         Approved apps: com.apple.TextEdit only.\n\
         Launch with --dangerously-skip-permissions.\n\
         Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", bypassed.to_str().unwrap()])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("capability-bypass"), "{stderr}");
    assert!(stderr.contains("D-045"), "{stderr}");

    // JSON mode reports the rules machine-readably.
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", bypassed.to_str().unwrap(), "--json"])
        .output()?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["ok"], false);
    assert!(
        report["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["rule"] == "capability-bypass")
    );
    Ok(())
}

/// Each of the three grant terms is required on its own: dropping any one is a
/// separate rule id, and the message names what to add.
#[test]
fn brief_lint_desktop_control_missing_terms() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let contract = "Reply on one line: DONE <sha> or BLOCKED <reason>.\n";

    for (name, head, rule) in [
        (
            "no-host",
            "Read the note in TextEdit using computer-use.\n\
             capability: computer-use\n\
             Approved apps: com.apple.TextEdit only.\n",
            "missing-host",
        ),
        (
            "no-bundle-id",
            "Read the note in TextEdit using computer-use.\n\
             capability: computer-use\n\
             host: hst_mac-mini\n",
            "missing-bundle-id",
        ),
        (
            "no-capability",
            "Read the note in TextEdit using computer-use.\n\
             host: hst_mac-mini\n\
             Approved apps: com.apple.TextEdit only.\n",
            "missing-capability",
        ),
    ] {
        let path = dir.path().join(format!("cua-{name}.md"));
        std::fs::write(&path, format!("{head}{contract}"))?;
        let output = std::process::Command::new(bin())
            .args(["brief", "lint", path.to_str().unwrap()])
            .output()?;
        assert_eq!(output.status.code(), Some(2), "{name} must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(rule), "{name}: {stderr}");
    }

    // A brief that names a domain but no app bundle id is still missing one:
    // a hostname is not an app.
    let domain_only = dir.path().join("cua-domain-only.md");
    std::fs::write(
        &domain_only,
        format!(
            "Read the note in TextEdit using computer-use.\n\
             capability: computer-use\n\
             host: hst_mac-mini\n\
             See docs.example.org for the vendor notes.\n\
             {contract}"
        ),
    )?;
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", domain_only.to_str().unwrap()])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing-bundle-id"), "{stderr}");
    Ok(())
}

/// `--force-lint` is the documented override: the same brief dispatches with
/// it. Proven at the lint layer rather than by a dispatch (which needs a Hub
/// and would not reach the capability preflight this task does not own).
#[test]
fn brief_lint_desktop_rule_is_overridable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cua-no-host.md");
    std::fs::write(
        &path,
        "Read the note in TextEdit using computer-use.\n\
         capability: computer-use\n\
         Approved apps: com.apple.TextEdit only.\n\
         Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;
    // `remuda brief lint` has no --force-lint (it only reports); the override
    // lives on dispatch. What this pins is that the rule is reported, not
    // silently swallowed, so the dispatcher has something to override.
    let output = std::process::Command::new(bin())
        .args(["brief", "lint", path.to_str().unwrap(), "--json"])
        .output()?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    let rules: Vec<&str> = report["violations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["rule"].as_str())
        .collect();
    assert_eq!(rules, vec!["missing-host"], "{report}");
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

/// Next inbound frame worth answering, or `None` once the stream closed.
///
/// An idle timeout is retried rather than ending the fake Node: the Hub
/// sends nothing between `runtime.hello` and the first dispatch RPC, while
/// the CLI subprocesses before that dispatch can sit longer than the frame
/// window on a loaded gate host. Letting the task exit dropped the socket,
/// and the Hub then marked the host offline — the 422
/// PLACEMENT_UNSATISFIABLE "host is offline" gate flake.
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
    spawn_hub_with_cli(None).await
}

/// Same fixture, with optional extra `cli[]` rows on the fake Node's heartbeat.
///
/// The capability cases need a Node that reports a `computer-use` row; the
/// default fake Node advertises only claude, which is the "not reported" shape.
async fn spawn_hub_with_cli(extra_cli: Option<Value>) -> Result<Hub> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("dispatch-cli").await?;
    let host = remuda_protocol::HostId::new().as_id().to_string();
    let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
    let mut cli = vec![json!({
        "kind": "claude", "version": "0.0.0",
        "absolutePath": "/usr/bin/claude", "authState": "unknown"
    })];
    if let Some(extra) = extra_cli {
        cli.push(extra);
    }

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
                    "cli": cli,
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
        while let Some(frame) = next_rpc_frame(&mut node).await {
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
    let base = format!("http://{}", hub.addr);
    let liveness = remuda_hub_client::HubClient::new(base.clone(), Some(token.clone()), None)?;
    // Wait for the hosts index to project the live link before any test
    // dispatches, instead of racing it with a fixed sleep. Paired with the
    // keepalive loop above this keeps the host online through the slow
    // pre-dispatch CLI sequence on a loaded gate host.
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
async fn unknown_model_pin_refuses_nonzero_with_message() -> Result<()> {
    let hub = spawn_hub().await?;

    // Project with a member workspace (same setup as the lifecycle test).
    let output = run(&["project", "create", "--name", "cli-pinrefuse"], &hub);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let project: Value = serde_json::from_slice(&output.stdout)?;
    let project_id = project["id"].as_str().unwrap().to_string();
    let client =
        remuda_hub_client::HubClient::new(hub.base.clone(), Some(hub.token.clone()), None)?;
    client
        .post(
            &format!("/v1/projects/{project_id}/members"),
            &json!({"hostId": hub.host, "workspaceId": hub.workspace, "role": "build"}),
        )
        .await?;
    client
        .patch(
            &format!("/v1/projects/{project_id}"),
            &json!({
                "hosts": [{
                    "hostId": hub.host, "maxInstances": 8, "maxBuilding": 4,
                    "diskBudgetGb": 10, "portBlocks": ["59000-59029"],
                    "requires": ["toolchain=rust"], "latencyClass": "remote",
                }],
            }),
        )
        .await?;
    client
        .post(
            "/v1/providers",
            &json!({
                "name": "cli-synth-relay",
                "kind": "gateway",
                "baseUrl": "http://127.0.0.1:1",
                "authToken": "sk-cli-synth-secret-eeee",
                "models": [
                    {"id": "passthrough/synth/seed-evolving", "family": "synth",
                     "role": "workhorse", "priority": 20}
                ]
            }),
        )
        .await?;

    let brief = hub._dir.path().join("pin-brief.md");
    std::fs::write(
        &brief,
        "Complete the trivial task in your worktree.\n\
         Rules: never run deploy/ scripts or probe tunnels.\n\
         Reply on one line: DONE <sha> or BLOCKED <reason>.\n",
    )?;

    // Dispatch with a pin no profile lists: non-zero exit, refusal reasons on
    // stderr naming the pin and the closest listed id.
    let output = run(
        &[
            "dispatch",
            "--project",
            &project_id,
            "--brief",
            brief.to_str().unwrap(),
            "--model",
            "synth/seed-evolving[1m]",
        ],
        &hub,
    );
    assert!(!output.status.success(), "unknown pin must exit non-zero");
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pin refused"), "{stderr}");
    assert!(stderr.contains("synth/seed-evolving[1m]"), "{stderr}");
    assert!(stderr.contains("never substituted"), "{stderr}");
    assert!(
        stderr.contains("passthrough/synth/seed-evolving"),
        "stderr must carry the suggestion: {stderr}"
    );

    // `profile probe` shows the same refusal (non-zero), not a 200 dry-run.
    let output = run(
        &["profile", "probe", "--pin-model", "synth/seed-evolving[1m]"],
        &hub,
    );
    assert!(!output.status.success(), "probe must refuse the pin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pin refused"), "{stderr}");
    assert!(
        stderr.contains("passthrough/synth/seed-evolving"),
        "{stderr}"
    );
    Ok(())
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
    // This fake node's `cli[]` holds only claude, so the capability row is
    // "not reported" — never a claim that the host cannot do it. The `error`
    // check matters: without it this assertion would also pass when the read
    // itself failed, which is a different state with a different remedy.
    assert_eq!(cap["computerUse"]["reported"], false);
    assert!(cap["computerUse"].get("installed").is_none());
    assert!(
        cap["computerUse"].get("error").is_none(),
        "an unreported row is not a read failure: {}",
        cap["computerUse"]
    );
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

// ── hostcap: the computer-use capability block ────────────────────────────
//
// The capability rides `/v1/hosts/{id}/hostcap` rather than a second read of
// `GET /v1/hosts/{id}`, because that route is operator-only. These cases drive
// the real path: a fake Node that reports the row, and — for the agent case —
// a caller that carries `REMUDA_INSTANCE_ID`, which is how every `remuda` run
// from inside a launched coordinator identifies itself.

/// Install a `computer-use` row on a fake Node's heartbeat.
fn installed_computer_use_row() -> Value {
    json!({
        "kind": "computer-use",
        "version": "2.7.0",
        "path": "/x/.codex/computer-use/SkyComputerUseClient",
        "auth": "unknown",
        "installed": true
    })
}

/// `remuda hostcap` from inside a launched coordinator.
///
/// This is the caller the verb exists for, and the one the operator-only host
/// route refuses: the run sets `REMUDA_INSTANCE_ID`, so the Hub classifies it
/// as agent origin. It must still see the capability.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostcap_reports_the_capability_to_an_agent_origin_caller() -> Result<()> {
    let hub = spawn_hub_with_cli(Some(installed_computer_use_row())).await?;

    // A claude instance holding `dispatch` is what an agent caller needs to
    // reach this route at all (require_grant reads the instance's grants).
    let body = json!({
        "hostId": hub.host,
        "kind": "claude",
        "driver": "claude-print",
        "delegation": "none",
        "grants": ["dispatch"],
    });
    let client =
        remuda_hub_client::HubClient::new(hub.base.clone(), Some(hub.token.clone()), None)?;
    let created = client
        .post("/v1/instances", &body)
        .await
        .context("create instance")?;
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?
        .to_string();

    let mut all = vec![
        "hostcap", &hub.host, "--hub", &hub.base, "--token", &hub.token,
    ];
    let output = std::process::Command::new(bin())
        .args(all.drain(..))
        .env("REMUDA_INSTANCE_ID", &instance_id)
        .output()
        .expect("run remuda");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cap: Value = serde_json::from_slice(&output.stdout)?;
    // The capability, visible to the agent — and no error key, which is what
    // the old second-GET implementation always produced for this caller.
    assert_eq!(cap["computerUse"]["installed"], true, "{cap}");
    assert_eq!(cap["computerUse"]["reported"], true, "{cap}");
    assert_eq!(cap["computerUse"]["version"], "2.7.0", "{cap}");
    assert!(
        cap["computerUse"].get("error").is_none(),
        "an agent caller must read the capability, not a refusal: {}",
        cap["computerUse"]
    );
    // The capacity keys still ride the same payload.
    assert_eq!(cap["cores"], 8);
    Ok(())
}

/// The reported-installed path, from an ordinary operator caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostcap_reports_an_installed_computer_use_row() -> Result<()> {
    let hub = spawn_hub_with_cli(Some(installed_computer_use_row())).await?;
    let output = run(&["hostcap", &hub.host], &hub);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cap: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(cap["computerUse"]["reported"], true, "{cap}");
    assert_eq!(cap["computerUse"]["installed"], true, "{cap}");
    assert_eq!(cap["computerUse"]["auth"], "unknown", "{cap}");
    assert!(cap["computerUse"].get("error").is_none(), "{cap}");
    assert!(
        cap["computerUse"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("SkyComputerUseClient")),
        "{cap}"
    );
    Ok(())
}
