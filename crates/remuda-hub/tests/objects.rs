//! D-027: attachment staging — auth, sniffing, limits, and the Node read path.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, InstanceId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

/// Smallest valid PNG this test needs: a real signature plus filler. The Hub
/// sniffs the signature and never decodes the image, so filler is enough.
fn png_bytes(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(len.max(bytes.len()), 0x5A);
    bytes
}

fn gif_bytes() -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&[0u8; 32]);
    bytes
}

/// Raw request with an arbitrary body and content type, which the JSON-only
/// helper in the other suites cannot express.
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
        if !content_type.is_empty() {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
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

async fn json_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let owned = body.map(|text| ("application/json", text.as_bytes()));
    let (status, head, bytes) = raw(addr, method, path, headers, owned).await?;
    Ok((status, head, String::from_utf8_lossy(&bytes).to_string()))
}

fn cookie_from(head: &str) -> Option<String> {
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

async fn login(
    addr: std::net::SocketAddr,
    bootstrap: &str,
    name: &str,
    kind: &str,
) -> Result<String> {
    let body =
        json!({"bootstrapToken": bootstrap, "deviceName": name, "deviceKind": kind}).to_string();
    let (status, head, rest) = json_request(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    cookie_from(&head).context("set-cookie")
}

async fn enroll_token(addr: std::net::SocketAddr, cookie: &str) -> Result<String> {
    let (status, _, rest) = json_request(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", cookie)],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "enroll-token {status} {rest}");
    let value: Value = serde_json::from_str(rest.trim())?;
    value["token"]
        .as_str()
        .map(str::to_string)
        .context("enroll token")
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = tokio::time::timeout(TIMEOUT, ws.next())
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

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    hub: remuda_hub::RunningHub,
    cookie: String,
    node: NodeSocket,
    node_token: String,
    host_id: String,
    instance_id: String,
    _dir: tempfile::TempDir,
}

/// Boot a Hub, pair a human device, enroll a Node, and project one running
/// instance onto that host via the journal — the state an upload needs.
async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap, "objects-phone", "human").await?;
    let enroll = enroll_token(addr, &cookie).await?;

    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    let host_id = HostId::new().as_id().as_str().to_owned();
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.1.0", "label":"objects-node"}})
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    let node_token = hello
        .pointer("/result/nodeToken")
        .and_then(Value::as_str)
        .context("node token")?
        .to_owned();

    let instance_id = InstanceId::new().as_id().as_str().to_owned();
    append(
        &mut node,
        "s1",
        &instance_id,
        json!({"kind":"lifecycle",
            "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                       "reasonCode":"driver-started"}}),
    )
    .await?;
    Ok(Fixture {
        hub,
        cookie,
        node,
        node_token,
        host_id,
        instance_id,
        _dir: dir,
    })
}

async fn append(
    node: &mut NodeSocket,
    rpc_id: &str,
    instance_id: &str,
    event: Value,
) -> Result<()> {
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":rpc_id, "method":"journal.append",
            "params":{"instanceId":instance_id, "event":event}})
        .to_string()
        .into(),
    ))
    .await?;
    // A live Node socket also receives forwarded Hub->Node commands, so read
    // past anything that is a request rather than our ack.
    loop {
        let frame = recv_json(node).await?;
        if frame.get("method").is_some() {
            continue;
        }
        anyhow::ensure!(frame.get("result").is_some(), "{frame}");
        return Ok(());
    }
}

async fn upload(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<(u16, String)> {
    let (status, _, body) = raw(
        addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", cookie)],
        Some((content_type, bytes)),
    )
    .await?;
    Ok((status, String::from_utf8_lossy(&body).to_string()))
}

#[tokio::test]
async fn upload_sniffs_the_type_and_returns_a_derived_name() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &png_bytes(512),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let value: Value = serde_json::from_str(body.trim())?;
    let object_id = value["objectId"].as_str().context("objectId")?;
    assert!(object_id.starts_with("obj_"), "{object_id}");
    assert_eq!(value["mediaType"], json!("image/png"));
    assert_eq!(value["size"], json!(512));
    assert_eq!(
        value["name"],
        json!(format!("{object_id}.png")),
        "the stored name is derived from the id, never from the caller"
    );
    assert_eq!(value["instanceId"], json!(fixture.instance_id));
    assert!(value["expiresAt"].as_str().is_some_and(|v| !v.is_empty()));
    fixture.hub.shutdown().await;
    Ok(())
}

/// The bytes decide the type. A caller that mislabels them is refused rather
/// than quietly believed.
#[tokio::test]
async fn upload_rejects_a_content_type_that_contradicts_the_bytes() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &gif_bytes(),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("disagrees"), "{body}");

    // An honest declaration for the same bytes is accepted.
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/gif",
        &gif_bytes(),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn upload_rejects_types_outside_the_image_allowlist() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    for (content_type, bytes) in [
        ("application/pdf", b"%PDF-1.7 fake".to_vec()),
        (
            "image/svg+xml",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
        ),
        ("text/plain", b"just text".to_vec()),
    ] {
        let (status, body) = upload(
            addr,
            &fixture.cookie,
            &fixture.instance_id,
            content_type,
            &bytes,
        )
        .await?;
        assert_eq!(status, 400, "{content_type} {body}");
        assert!(body.contains("PNG, JPEG, GIF or WebP"), "{body}");
    }
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn upload_rejects_an_attachment_over_the_size_cap() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let oversized = png_bytes(5 * 1024 * 1024 + 1);
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &oversized,
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("RESOURCE_LIMIT"), "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

/// D-027: the MVP gives Agent-origin callers no upload channel at all.
#[tokio::test]
async fn agent_origin_cannot_upload_and_human_can() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    // Mint an instance-scoped credential, which is exactly what an in-session
    // agent uses, then present it on an upload.
    let (status, _, body) = json_request(
        addr,
        "POST",
        &format!("/v1/instances/{}/mcp-token", fixture.instance_id),
        &[("Cookie", fixture.cookie.as_str())],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "mcp-token {status} {body}");
    let agent_token = serde_json::from_str::<Value>(body.trim())?["token"]
        .as_str()
        .context("agent token")?
        .to_owned();

    let (status, body) = {
        let (status, _, body) = raw(
            addr,
            "POST",
            &format!("/v1/objects?instanceId={}", fixture.instance_id),
            &[("Authorization", &format!("Bearer {agent_token}"))],
            Some(("image/png", &png_bytes(256))),
        )
        .await?;
        (status, String::from_utf8_lossy(&body).to_string())
    };
    assert_eq!(status, 403, "an agent must not stage attachments: {body}");

    // A bot device is an operator for this purpose and is allowed.
    let bot = login(addr, &fixture.hub.bootstrap_token, "objects-bot", "bot").await?;
    let (status, body) = upload(
        addr,
        &bot,
        &fixture.instance_id,
        "image/png",
        &png_bytes(256),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn upload_requires_authentication_and_a_known_instance() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let (status, _, _) = raw(
        addr,
        "POST",
        &format!("/v1/objects?instanceId={}", fixture.instance_id),
        &[],
        Some(("image/png", &png_bytes(128))),
    )
    .await?;
    assert_eq!(status, 401, "an anonymous upload must not be staged");

    let (status, body) = upload(
        addr,
        &fixture.cookie,
        "ins_missing",
        "image/png",
        &png_bytes(128),
    )
    .await?;
    assert_eq!(status, 404, "{body}");

    // No instanceId at all: the route cannot bind the object to anything.
    let (status, _, body) = raw(
        addr,
        "POST",
        "/v1/objects",
        &[("Cookie", fixture.cookie.as_str())],
        Some(("image/png", &png_bytes(128))),
    )
    .await?;
    assert!(
        matches!(status, 400 | 422),
        "{status} {}",
        String::from_utf8_lossy(&body)
    );
    fixture.hub.shutdown().await;
    Ok(())
}

/// Identical bytes for one instance stage once, so a retried upload cannot
/// inflate the instance's quota.
#[tokio::test]
async fn re_uploading_identical_bytes_reuses_the_object() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let bytes = png_bytes(1024);
    let (_, first) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &bytes,
    )
    .await?;
    let (_, second) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &bytes,
    )
    .await?;
    let first: Value = serde_json::from_str(first.trim())?;
    let second: Value = serde_json::from_str(second.trim())?;
    assert_eq!(first["objectId"], second["objectId"]);
    assert_eq!(first["digest"], second["digest"]);
    fixture.hub.shutdown().await;
    Ok(())
}

/// The Node pulls with its host token; a different host's token must not work.
#[tokio::test]
async fn the_owning_node_can_read_bytes_and_another_host_cannot() -> Result<()> {
    let mut fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let bytes = png_bytes(777);
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &bytes,
    )
    .await?;
    anyhow::ensure!(status == 200, "{body}");
    let object_id = serde_json::from_str::<Value>(body.trim())?["objectId"]
        .as_str()
        .context("objectId")?
        .to_owned();

    let (status, head, pulled) = raw(
        addr,
        "GET",
        &format!("/v1/objects/{object_id}"),
        &[("Authorization", &format!("Bearer {}", fixture.node_token))],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{head}");
    assert_eq!(pulled, bytes, "the Node must get the exact staged bytes");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("content-type: image/png"), "{head}");
    assert!(lower.contains("x-content-type-options: nosniff"), "{head}");
    assert!(lower.contains("content-disposition: attachment"), "{head}");

    // A second Node on the same Hub hosts nothing here and must be refused.
    let other_enroll = enroll_token(addr, &fixture.cookie).await?;
    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {other_enroll}").parse()?);
    let (mut other, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    other
        .send(Message::Text(
            json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params":{"hostId": HostId::new().as_id().as_str(), "nodeVersion":"0.1.0"}})
            .to_string()
            .into(),
        ))
        .await?;
    let other_token = recv_json(&mut other)
        .await?
        .pointer("/result/nodeToken")
        .and_then(Value::as_str)
        .context("other node token")?
        .to_owned();
    let (status, _, _) = raw(
        addr,
        "GET",
        &format!("/v1/objects/{object_id}"),
        &[("Authorization", &format!("Bearer {other_token}"))],
        None,
    )
    .await?;
    assert_eq!(
        status, 403,
        "a Node must not read another host's attachment"
    );

    // Anonymous reads are refused outright.
    let (status, _, _) = raw(addr, "GET", &format!("/v1/objects/{object_id}"), &[], None).await?;
    assert_eq!(status, 401);

    let _ = &mut fixture.node;
    fixture.hub.shutdown().await;
    Ok(())
}

/// A send may only reference objects staged for that same instance, and the
/// Hub rewrites the metadata from its own row rather than trusting the client.
#[tokio::test]
async fn send_accepts_only_attachments_bound_to_that_instance() -> Result<()> {
    let mut fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &png_bytes(321),
    )
    .await?;
    anyhow::ensure!(status == 200, "{body}");
    let object_id = serde_json::from_str::<Value>(body.trim())?["objectId"]
        .as_str()
        .context("objectId")?
        .to_owned();

    let command = json!({
        "operation": "instance.send",
        "payload": {
            "prompt": "what colour is the image?",
            "attachments": [{
                "objectId": object_id,
                "mediaType": "image/gif",
                "name": "../../escape.gif",
                "size": 99_999_999,
            }],
        }
    })
    .to_string();
    let (status, _, body) = json_request(
        addr,
        "POST",
        &format!("/v1/instances/{}/commands", fixture.instance_id),
        &[("Cookie", fixture.cookie.as_str())],
        Some(&command),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let queued: Value = serde_json::from_str(body.trim())?;
    let attachment = &queued["command"]["payload"]["attachments"][0];
    assert_eq!(attachment["objectId"], json!(object_id));
    assert_eq!(
        attachment["mediaType"],
        json!("image/png"),
        "the Hub must overwrite a client-claimed media type with the sniffed one"
    );
    assert_eq!(
        attachment["name"],
        json!(format!("{object_id}.png")),
        "a caller-supplied name must never survive into the command"
    );
    assert_eq!(attachment["size"], json!(321));

    // An unknown id is refused.
    let command = json!({
        "operation": "instance.send",
        "payload": {"prompt":"hi", "attachments":[{"objectId":"obj_unknown"}]}
    })
    .to_string();
    let (status, _, body) = json_request(
        addr,
        "POST",
        &format!("/v1/instances/{}/commands", fixture.instance_id),
        &[("Cookie", fixture.cookie.as_str())],
        Some(&command),
    )
    .await?;
    assert_eq!(status, 400, "{body}");

    // So is an object staged for a different instance.
    let other_instance = InstanceId::new().as_id().as_str().to_owned();
    append(
        &mut fixture.node,
        "s2",
        &other_instance,
        json!({"kind":"lifecycle",
            "payload":{"type":"entity", "entityType":"instance", "state":"ready"}}),
    )
    .await?;
    let command = json!({
        "operation": "instance.send",
        "payload": {"prompt":"hi", "attachments":[{"objectId": object_id}]}
    })
    .to_string();
    let (status, _, body) = json_request(
        addr,
        "POST",
        &format!("/v1/instances/{other_instance}/commands"),
        &[("Cookie", fixture.cookie.as_str())],
        Some(&command),
    )
    .await?;
    assert_eq!(
        status, 403,
        "an object staged elsewhere must not attach here: {body}"
    );
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn send_refuses_more_attachments_than_the_per_message_cap() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let mut ids = Vec::new();
    for index in 0..5u8 {
        let mut bytes = png_bytes(64);
        bytes.push(index);
        let (status, body) = upload(
            addr,
            &fixture.cookie,
            &fixture.instance_id,
            "image/png",
            &bytes,
        )
        .await?;
        anyhow::ensure!(status == 200, "{body}");
        ids.push(json!({
            "objectId": serde_json::from_str::<Value>(body.trim())?["objectId"]
                .as_str()
                .context("objectId")?
                .to_owned()
        }));
    }
    let command = json!({
        "operation": "instance.send",
        "payload": {"prompt":"too many", "attachments": ids}
    })
    .to_string();
    let (status, _, body) = json_request(
        addr,
        "POST",
        &format!("/v1/instances/{}/commands", fixture.instance_id),
        &[("Cookie", fixture.cookie.as_str())],
        Some(&command),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("RESOURCE_LIMIT"), "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

/// Staged bytes do not outlive the instance they were staged for.
#[tokio::test]
async fn exiting_an_instance_drops_its_staged_attachments() -> Result<()> {
    let mut fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let (status, body) = upload(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &png_bytes(200),
    )
    .await?;
    anyhow::ensure!(status == 200, "{body}");
    let object_id = serde_json::from_str::<Value>(body.trim())?["objectId"]
        .as_str()
        .context("objectId")?
        .to_owned();

    let pull = |id: String, token: String| async move {
        raw(
            addr,
            "GET",
            &format!("/v1/objects/{id}"),
            &[("Authorization", &format!("Bearer {token}"))],
            None,
        )
        .await
    };
    let (status, _, _) = pull(object_id.clone(), fixture.node_token.clone()).await?;
    assert_eq!(status, 200);

    append(
        &mut fixture.node,
        "exit",
        &fixture.instance_id,
        json!({"kind":"lifecycle",
            "payload":{"type":"entity", "entityType":"instance", "state":"exited",
                       "reasonCode":"closed"}}),
    )
    .await?;
    let (status, _, _) = pull(object_id, fixture.node_token.clone()).await?;
    assert_eq!(
        status, 404,
        "an exited instance must not leave attachments readable"
    );
    let _ = &fixture.host_id;
    fixture.hub.shutdown().await;
    Ok(())
}
