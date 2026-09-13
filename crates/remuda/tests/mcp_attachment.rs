//! D-028 §4.5 end to end: a real `remuda mcp` process, holding the
//! instance-scoped credential a session's MCP server gets, fetches a staged
//! image from a real Hub and receives it as an MCP `image` content block.
//!
//! The unit tests in `cmd::mcp::attachment` cover the mapping against a fake
//! Hub; this covers the wiring none of them can: the Agent-origin middleware,
//! the real route, the real credential, and a separate process.
//!
//! Run with `--nocapture` to print the transcript recorded in
//! `docs/design/evidence/attachments-2.md`.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

/// 1x1-ish PNG: a real signature plus filler. The Hub sniffs the signature and
/// never decodes the image.
fn png_bytes(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(len.max(bytes.len()), 0x5A);
    bytes
}

async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, &[u8])>,
) -> Result<(u16, String, Vec<u8>)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some((content_type, bytes)) = body {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
        head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some((_, bytes)) = body {
        request.extend_from_slice(bytes);
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("no header terminator"))?;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Ok((status, head, buf[split + 4..].to_vec()))
}

async fn post_json(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<Value> {
    let (status, _, bytes) = raw(
        addr,
        "POST",
        path,
        headers,
        Some(("application/json", body.as_bytes())),
    )
    .await?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    anyhow::ensure!(status == 200, "{path} -> {status} {text}");
    Ok(serde_json::from_str(text.trim())?)
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = timeout(TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("ws timeout"))?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => return Err(anyhow!("unexpected frame {other:?}")),
        }
    }
}

/// Drive `remuda mcp` over NDJSON with an instance-bound credential.
async fn mcp_session(
    hub: &str,
    token: &str,
    instance_id: &str,
    requests: &[Value],
) -> Result<Vec<Value>> {
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("REMUDA_HUB", hub)
        .env("REMUDA_TOKEN", token)
        .env("REMUDA_INSTANCE_ID", instance_id)
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
    for request in requests {
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
    }
    drop(stdin);

    let mut buf = Vec::new();
    let read = timeout(Duration::from_secs(15), async {
        let mut tmp = [0u8; 8192];
        loop {
            let n = stdout.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);
            if text.lines().filter(|line| !line.is_empty()).count() >= requests.len() {
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
    read.map_err(|_| anyhow!("mcp timeout; stderr={}", String::from_utf8_lossy(&err)))??;

    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    anyhow::ensure!(
        lines.len() >= requests.len(),
        "{} frames, want {}; stderr={}",
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

/// Boot a Hub, enroll a Node, stage an image against a live session, then let
/// a real `remuda mcp` process fetch it with that session's own credential.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_process_fetches_its_session_image_as_a_content_block() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let hub_url = format!("http://{addr}");

    // A human device stages the attachment, exactly as the browser does.
    let login = post_json(
        addr,
        "/v1/login",
        &[],
        &json!({"bootstrapToken": bootstrap, "deviceName":"evidence-phone",
                "deviceKind":"human"})
        .to_string(),
    )
    .await?;
    let human = login["token"].as_str().context("device token")?.to_owned();
    let human_auth = format!("Bearer {human}");

    // Enroll a Node and project one ready instance onto it.
    let enroll = post_json(
        addr,
        "/v1/hosts/enroll-token",
        &[("Authorization", human_auth.as_str())],
        "{}",
    )
    .await?["token"]
        .as_str()
        .context("enroll token")?
        .to_owned();
    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
               "params":{"hostId": HostId::new().as_id().as_str(), "nodeVersion":"0.1.0",
                         "label":"evidence-node"}})
        .to_string()
        .into(),
    ))
    .await?;
    anyhow::ensure!(
        recv_json(&mut node)
            .await?
            .pointer("/result/nodeToken")
            .is_some()
    );

    let instance_id = InstanceId::new().as_id().as_str().to_owned();
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"s1", "method":"journal.append",
               "params":{"instanceId": instance_id, "event":{"kind":"lifecycle",
                   "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                              "reasonCode":"driver-started"}}}})
        .to_string()
        .into(),
    ))
    .await?;
    loop {
        let frame = recv_json(&mut node).await?;
        if frame.get("method").is_none() {
            anyhow::ensure!(frame.get("result").is_some(), "{frame}");
            break;
        }
    }

    // Stage the image (D-027) and mint the session's MCP credential.
    let image = png_bytes(168);
    let (status, _, body) = raw(
        addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Authorization", human_auth.as_str())],
        Some(("image/png", &image)),
    )
    .await?;
    let staged: Value = serde_json::from_slice(&body)?;
    anyhow::ensure!(status == 200, "upload {status} {staged}");
    let object_id = staged["objectId"].as_str().context("objectId")?.to_owned();

    let agent_token = post_json(
        addr,
        &format!("/v1/instances/{instance_id}/mcp-token"),
        &[("Authorization", human_auth.as_str())],
        "{}",
    )
    .await?["token"]
        .as_str()
        .context("agent token")?
        .to_owned();

    let requests = vec![
        json!({"jsonrpc":"2.0","id":1,"method":"initialize",
               "params":{"protocolVersion":"2024-11-05","capabilities":{},
                         "clientInfo":{"name":"fake-agent","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
               "params":{"name":"remuda_attachments_list","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
               "params":{"name":"remuda_attachment","arguments":{"objectId": object_id}}}),
        // A forged id from another session is refused by the Hub, not guessed at.
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
               "params":{"name":"remuda_attachments_list",
                         "arguments":{"instanceId":"ins_someone_else"}}}),
    ];
    let replies = mcp_session(&hub_url, &agent_token, &instance_id, &requests).await?;

    assert_eq!(replies[0]["result"]["serverInfo"]["name"], json!("remuda"));

    let listed: Value = serde_json::from_str(
        replies[1]["result"]["content"][0]["text"]
            .as_str()
            .context("list text")?,
    )?;
    assert_eq!(
        replies[1]["result"]["isError"],
        json!(false),
        "{}",
        replies[1]
    );
    assert_eq!(listed["instanceId"], json!(instance_id));
    assert_eq!(listed["count"], json!(1));
    assert_eq!(listed["items"][0]["objectId"], json!(object_id));
    assert_eq!(listed["items"][0]["mediaType"], json!("image/png"));
    assert_eq!(listed["items"][0]["size"], json!(168));

    let fetched = &replies[2]["result"];
    assert_eq!(fetched["isError"], json!(false), "{fetched}");
    assert_eq!(fetched["content"][0]["type"], json!("image"));
    assert_eq!(fetched["content"][0]["mimeType"], json!("image/png"));
    let data = fetched["content"][0]["data"].as_str().context("data")?;
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(data)?;
    assert_eq!(
        decoded, image,
        "the agent must receive the exact staged bytes"
    );

    let refused = &replies[3]["result"];
    assert_eq!(refused["isError"], json!(true), "{refused}");
    let text = refused["content"][0]["text"].as_str().context("text")?;
    assert!(text.contains("own session"), "{text}");

    // Transcript for docs/design/evidence/attachments-2.md (`-- --nocapture`).
    println!("--- fake-agent MCP transcript ---");
    println!("instanceId = {instance_id}");
    println!("objectId   = {object_id}");
    for (request, reply) in requests.iter().zip(&replies) {
        println!("-> {request}");
        println!("<- {}", elide_base64(reply));
    }

    hub.shutdown().await;
    Ok(())
}

/// Replace a base64 payload with its head and length so the transcript stays
/// readable and the doc quotes something a reader can check.
fn elide_base64(reply: &Value) -> String {
    let mut reply = reply.clone();
    if let Some(data) = reply
        .pointer_mut("/result/content/0/data")
        .and_then(|value| value.as_str().map(str::to_owned))
    {
        reply["result"]["content"][0]["data"] =
            json!(format!("{}…<{} base64 chars>", &data[..24], data.len()));
    }
    reply.to_string()
}
