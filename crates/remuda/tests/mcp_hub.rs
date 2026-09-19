//! `remuda mcp` tools/list + tools/call instance.create against an in-process Hub
//! and a fake Node WebSocket client. No remuda-node process and no native CLI.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const RPC: Duration = Duration::from_secs(8);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = timeout(RPC, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match msg {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected ws frame {other:?}")),
        }
    }
}

/// D-018: a Node enrolls with a single-use enroll token, not the device
/// pairing access code. The in-process Hub mints one directly.
async fn enroll_fake_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
) -> Result<tokio::task::JoinHandle<()>> {
    enroll_fake_node_with_cli(
        hub,
        host_id,
        json!([{
            "kind": "claude",
            "version": "0.0.0",
            "absolutePath": "/usr/bin/claude",
            "authState": "unknown"
        }]),
    )
    .await
}

async fn enroll_fake_node_with_cli(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    cli: Value,
) -> Result<tokio::task::JoinHandle<()>> {
    let addr = hub.addr;
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": "fake-node",
                "host": {
                    "hostname": "fake-node.local",
                    "labels": { "region": "sg" },
                    "maxInstances": 4,
                    "cli": cli
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(
        hello["result"]["nodeToken"].as_str().is_some(),
        "hello {hello}"
    );
    Ok(tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if let Some(id) = frame.get("id").cloned()
                && frame.get("method").is_some()
            {
                let result = if frame["method"] == "host.doctor" {
                    json!({"exitCode":0,"inventory":{},"checks":[{"name":"fixture.remote","status":"ok","message":"Node diagnostics reached","details":null}]})
                } else {
                    json!({"ok":true})
                };
                let _ = node
                    .send(Message::Text(
                        json!({ "jsonrpc": "2.0", "id": id, "result": result })
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    }))
}

async fn mcp_roundtrip(hub: &str, bootstrap: &str, requests: &[Value]) -> Result<Vec<Value>> {
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("REMUDA_HUB", hub)
        .env("REMUDA_BOOTSTRAP_TOKEN", bootstrap)
        .env_remove("REMUDA_TOKEN")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn remuda mcp")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let mut stdout = child.stdout.take().context("stdout")?;
    for req in requests {
        stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .context("write mcp")?;
    }
    drop(stdin);

    let mut buf = Vec::new();
    let read = timeout(Duration::from_secs(10), async {
        let mut tmp = [0u8; 8192];
        loop {
            let n = stdout.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);
            if text.lines().filter(|l| !l.is_empty()).count() >= requests.len() {
                break;
            }
        }
        anyhow::Ok(())
    })
    .await;
    let mut err = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = timeout(Duration::from_millis(200), stderr.read_to_end(&mut err)).await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    read.map_err(|_| {
        anyhow!(
            "mcp stdout timeout stderr={}",
            String::from_utf8_lossy(&err)
        )
    })??;
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    anyhow::ensure!(
        lines.len() >= requests.len(),
        "mcp frames {} want {} stdout={text:?} stderr={}",
        lines.len(),
        requests.len(),
        String::from_utf8_lossy(&err)
    );
    lines
        .iter()
        .take(requests.len())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tools_list_and_instance_create_against_in_process_hub() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let bootstrap = hub.bootstrap_token.clone();
    let host_id = HostId::new();
    let node = enroll_fake_node(&hub, host_id.as_id().as_str()).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let hub_url = format!("http://{}", hub.addr);
    let replies = mcp_roundtrip(
        &hub_url,
        &bootstrap,
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "mcp-hub-test", "version": "0" }
                }
            }),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "remuda_instance_create",
                    "arguments": {
                        "host": host_id.as_id().as_str(),
                        "kind": "claude",
                        "driver": "claude-print",
                        "prompt": "ok"
                    }
                }
            }),
        ],
    )
    .await?;

    assert_eq!(replies[0]["result"]["serverInfo"]["name"], json!("remuda"));
    let names: Vec<String> = replies[1]["result"]["tools"]
        .as_array()
        .context("tools")?
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.contains(&"remuda_instance_create".into()),
        "tools {names:?}"
    );
    assert!(
        names.contains(&"remuda_worktree_create".into()),
        "tools {names:?}"
    );
    assert!(
        names.contains(&"remuda_fleet_send".into()),
        "tools {names:?}"
    );
    assert!(
        names.contains(&"remuda_instance_keys".into()),
        "tools {names:?}"
    );

    let call = &replies[2];
    assert_eq!(call["result"]["isError"], json!(false), "{call}");
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .context("tool text")?;
    let created: Value = serde_json::from_str(text).context("create json")?;
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .context("instanceId")?;
    assert!(instance_id.starts_with("ins_"), "instanceId {instance_id}");
    assert_eq!(
        created["instance"]["hostId"].as_str(),
        Some(host_id.as_id().as_str())
    );

    node.abort();
    Ok(())
}

/// D-045 regression (round 3): GET /v1/hosts is operator-only. An
/// agent-origin MCP `remuda_instance_create` (instance-scoped credential)
/// must still succeed without capabilities (no client-side host listing),
/// while a create WITH a capability fails loudly rather than silently
/// skipping the unreadable host inventory.
#[tokio::test(flavor = "multi_thread")]
async fn agent_scoped_mcp_create_skips_host_listing_but_capability_refuses() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let host_id = HostId::new();
    // The host itself qualifies for computer-use (macOS, installed row); only
    // the agent token's inability to list it is what we test here.
    let node = enroll_fake_node_with_cli(
        &hub,
        host_id.as_id().as_str(),
        json!([
            {"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"},
            {"kind":"computer-use","installed":true,
             "path":"/Users/u/.codex/computer-use/Codex Computer Use.app","auth":"unknown"}
        ]),
    )
    .await?;
    hub.test_insert_host(host_id.as_id().as_str()).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // A pre-existing instance whose token authenticates as InputOrigin::Agent
    // and whose delegation grants Dispatch (as a delegated worker has;
    // agent_scope::prepare_create otherwise refuses before host listing).
    let project_id = remuda_protocol::ProjectId::new();
    let mut delegation =
        remuda_hub::store_test_support::leaf_delegation(project_id.as_id().as_str())
            .expect("leaf delegation");
    delegation.grants = vec!["dispatch".to_owned()];
    let project = serde_json::json!({ "projectId": project_id.as_id() });
    let instance = hub
        .store()
        .unwrap()
        .insert_instance_delegated(
            host_id.as_id().to_string(),
            None,
            "claude".into(),
            "claude-print".into(),
            None,
            project,
            delegation,
        )
        .await
        .map_err(|error| anyhow!("seed instance: {error}"))?;
    let agent_token =
        remuda_hub::instance_token(hub.store().unwrap().clone(), instance.instance_id.clone())
            .await
            .map_err(anyhow::Error::from)?;
    let hub_url = format!("http://{}", hub.addr);

    // Sanity: the agent token really is denied the host listing the
    // preflight needs. Driven through a CLI verb that lists hosts (`host
    // files` resolves the host id via GET /v1/hosts), which exits nonzero
    // with the 403 rather than returning a host.
    let list = Command::new(bin())
        .args([
            "host",
            "files",
            "ls",
            host_id.as_id().as_str(),
            "ws-does-not-matter",
        ])
        .env("REMUDA_HUB", &hub_url)
        .env("REMUDA_TOKEN", &agent_token)
        .output()
        .await?;
    let list_output = format!(
        "{}{}",
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(!list.status.success(), "agent token must not list hosts");
    assert!(
        list_output.contains("403"),
        "expected a 403 on agent host list: {list_output}"
    );

    // No capability: the MCP create reaches the Hub and succeeds despite the
    // 403-only host listing (preflight is skipped on the non-capability path).
    let ok_replies = mcp_roundtrip_with_token(
        &hub_url,
        &agent_token,
        &[json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "remuda_instance_create",
                "arguments": {
                    "host": host_id.as_id().as_str(),
                    "kind": "claude",
                    "driver": "claude-print",
                    "prompt": "ok"
                }
            }
        })],
    )
    .await?;
    assert_eq!(
        ok_replies[0]["result"]["isError"],
        json!(false),
        "agent create without capabilities must succeed: {}",
        ok_replies[0]
    );

    // With computer-use against a host NOT resolvable from the agent's
    // (forbidden) inventory: the CLI cannot run its preflight and must fail
    // loudly instead of dispatching — it cannot compare the unread inventory,
    // so an unknown target is refused at the client with a retry direction.
    let guarded_replies = mcp_roundtrip_with_token(
        &hub_url,
        &agent_token,
        &[json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "remuda_instance_create",
                "arguments": {
                    "host": "hst_00000000-0000-7000-8000-0000000000ff",
                    "kind": "claude",
                    "driver": "claude-print",
                    "prompt": "ok",
                    "capabilities": ["computer-use"]
                }
            }
        })],
    )
    .await?;
    assert_eq!(
        guarded_replies[0]["result"]["isError"],
        json!(true),
        "capability create with an unreadable host list must fail loudly: {}",
        guarded_replies[0]
    );
    let error_text = guarded_replies[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        error_text.contains("preflight")
            || error_text.contains("host inventory")
            || error_text.contains("not in the Hub"),
        "error must name the skipped preflight / missing inventory: {error_text}"
    );
    // Crucially the request never reached persistence: no instance for that
    // unknown host was created.
    let list = Command::new(bin())
        .args(["instance", "list", "--json"])
        .env("REMUDA_HUB", &hub_url)
        .env("REMUDA_BOOTSTRAP_TOKEN", hub.bootstrap_token.clone())
        .output()
        .await?;
    let list_text = String::from_utf8_lossy(&list.stdout);
    assert!(
        !list_text.contains("hst_00000000-0000-7000-8000-0000000000ff"),
        "refused create must not have been persisted: {list_text}"
    );

    node.abort();
    Ok(())
}

/// Like [`mcp_roundtrip`] but with an explicit bearer token (instance-scoped
/// agent credential instead of the bootstrap operator token).
async fn mcp_roundtrip_with_token(
    hub: &str,
    token: &str,
    requests: &[Value],
) -> Result<Vec<Value>> {
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("REMUDA_HUB", hub)
        .env("REMUDA_TOKEN", token)
        .env_remove("REMUDA_BOOTSTRAP_TOKEN")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn remuda mcp")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let mut stdout = child.stdout.take().context("stdout")?;
    for req in requests {
        stdin.write_all(format!("{req}\n").as_bytes()).await?;
    }
    drop(stdin);

    let mut buf = Vec::new();
    let read = timeout(Duration::from_secs(10), async {
        let mut tmp = [0u8; 8192];
        loop {
            let n = stdout.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);
            if text.lines().filter(|l| !l.is_empty()).count() >= requests.len() {
                break;
            }
        }
        anyhow::Ok(())
    })
    .await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    read??;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .filter(|l| !l.is_empty())
        .take(requests.len())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_routes_to_the_registered_host_and_mcp_marks_offline_hosts_as_errors() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let host_id = HostId::new();
    let node = enroll_fake_node(&hub, host_id.as_id().as_str()).await?;
    let hub_url = format!("http://{}", hub.addr);
    let config = dir.path().join("remuda.toml");
    std::fs::write(
        &config,
        "[hub]\nlisten = '127.0.0.1:0'\n[node]\nlisten = '127.0.0.1:0'\n",
    )?;
    let output = timeout(
        RPC,
        Command::new(bin())
            .args([
                "doctor",
                "--host",
                host_id.as_id().as_str(),
                "--json",
                "--hub",
                &hub_url,
            ])
            .env("REMUDA_BOOTSTRAP_TOKEN", &hub.bootstrap_token)
            .env_remove("REMUDA_TOKEN")
            .env("REMUDA_DATA_DIR", dir.path().join("client"))
            .env("REMUDA_CONFIG", &config)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let report: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| String::from_utf8_lossy(&output.stderr).to_string())?;
    assert!(output.status.success(), "{report}");
    assert_eq!(report["mode"], "remote");
    assert_eq!(report["checks"][0]["name"], "fixture.remote");
    assert_eq!(report["registeredHosts"][0]["online"], true);
    let call = |id: &str| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"remuda_doctor","arguments":{"host":id}}});
    let replies = mcp_roundtrip(
        &hub_url,
        &hub.bootstrap_token,
        &[
            call(host_id.as_id().as_str()),
            call(HostId::new().as_id().as_str()),
        ],
    )
    .await?;
    assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
    assert_eq!(
        replies[0]["result"]["structuredContent"]["checks"][0]["name"],
        "fixture.remote"
    );
    assert_eq!(replies[1]["result"]["isError"], true);
    assert_eq!(replies[1]["result"]["structuredContent"]["exitCode"], 1);
    node.abort();
    node.await.ok();
    hub.shutdown().await;
    Ok(())
}
