//! D-028 §4.5: the session-scoped attachment reads the in-session MCP server
//! uses. D-027's staging routes are covered by `objects.rs` and untouched here.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
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
/// Mirrors `attachments::MAX_INLINE_ATTACHMENT_BYTES`.
const INLINE_CAP: usize = 3_584 * 1024;

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

/// `GET` with a Bearer token, parsed as JSON when the body is non-empty.
async fn get_as(
    addr: std::net::SocketAddr,
    path: &str,
    token: &str,
) -> Result<(u16, Option<Value>)> {
    let (status, _, body) = raw(
        addr,
        "GET",
        path,
        &[("Authorization", &format!("Bearer {token}"))],
        None,
    )
    .await?;
    let text = String::from_utf8_lossy(&body).to_string();
    let parsed = serde_json::from_str(text.trim()).ok();
    Ok((status, parsed))
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
    /// The session under test, and an instance-scoped credential bound to it.
    instance_id: String,
    agent_token: String,
    /// A second live session on the same host, for the wrong-session cases.
    other_instance_id: String,
    other_agent_token: String,
    _dir: tempfile::TempDir,
}

/// Boot a Hub, pair a human, enroll a Node, project two ready instances, and
/// mint an Agent credential for each — the shape an in-session MCP server has.
async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap, "attachments-phone", "human").await?;

    let (status, _, body) = json_request(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", cookie.as_str())],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "enroll-token {status} {body}");
    let enroll = serde_json::from_str::<Value>(body.trim())?["token"]
        .as_str()
        .context("enroll token")?
        .to_owned();

    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    let host_id = HostId::new().as_id().as_str().to_owned();
    node.send(Message::Text(
        json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params":{"hostId": host_id, "nodeVersion":"0.1.0", "label":"attachments-node"}})
        .to_string()
        .into(),
    ))
    .await?;
    let node_token = recv_json(&mut node)
        .await?
        .pointer("/result/nodeToken")
        .and_then(Value::as_str)
        .context("node token")?
        .to_owned();

    let instance_id = InstanceId::new().as_id().as_str().to_owned();
    let other_instance_id = InstanceId::new().as_id().as_str().to_owned();
    for (rpc_id, id) in [("s1", &instance_id), ("s2", &other_instance_id)] {
        append(
            &mut node,
            rpc_id,
            id,
            json!({"kind":"lifecycle",
                "payload":{"type":"entity", "entityType":"instance", "state":"ready",
                           "reasonCode":"driver-started"}}),
        )
        .await?;
    }
    let agent_token = mcp_token(addr, &cookie, &instance_id).await?;
    let other_agent_token = mcp_token(addr, &cookie, &other_instance_id).await?;
    Ok(Fixture {
        hub,
        cookie,
        node,
        node_token,
        instance_id,
        agent_token,
        other_instance_id,
        other_agent_token,
        _dir: dir,
    })
}

/// The instance-scoped credential the Node hands the in-session MCP server.
async fn mcp_token(addr: std::net::SocketAddr, cookie: &str, instance_id: &str) -> Result<String> {
    let (status, _, body) = json_request(
        addr,
        "POST",
        &format!("/v1/instances/{instance_id}/mcp-token"),
        &[("Cookie", cookie)],
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "mcp-token {status} {body}");
    Ok(serde_json::from_str::<Value>(body.trim())?["token"]
        .as_str()
        .context("agent token")?
        .to_owned())
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
    loop {
        let frame = recv_json(node).await?;
        if frame.get("method").is_some() {
            continue;
        }
        anyhow::ensure!(frame.get("result").is_some(), "{frame}");
        return Ok(());
    }
}

/// Stage one attachment through the D-027 route and return its id.
async fn stage(
    addr: std::net::SocketAddr,
    cookie: &str,
    instance_id: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String> {
    let (status, _, body) = raw(
        addr,
        "POST",
        &format!("/v1/objects?instanceId={instance_id}"),
        &[("Cookie", cookie)],
        Some((content_type, bytes)),
    )
    .await?;
    let text = String::from_utf8_lossy(&body).to_string();
    anyhow::ensure!(status == 200, "upload {status} {text}");
    Ok(serde_json::from_str::<Value>(text.trim())?["objectId"]
        .as_str()
        .context("objectId")?
        .to_owned())
}

/// The whole point of D-028 §4.5: the credential the agent actually holds can
/// read the image bytes, which `GET /v1/objects/{id}` refuses it outright.
#[tokio::test]
async fn an_instance_credential_reads_its_own_attachment_as_base64() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let bytes = png_bytes(168);
    let object_id = stage(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &bytes,
    )
    .await?;

    // D-027's route is closed to this credential; that is why §4.5 exists.
    let (status, _) = get_as(
        addr,
        &format!("/v1/objects/{object_id}"),
        &fixture.agent_token,
    )
    .await?;
    assert_eq!(
        status, 403,
        "the D-027 read must stay operator/Node only, or §4.5 has no purpose"
    );

    let (status, body) = get_as(
        addr,
        &format!("/v1/attachments/{object_id}/content"),
        &fixture.agent_token,
    )
    .await?;
    let body = body.context("json body")?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["objectId"], json!(object_id));
    assert_eq!(body["instanceId"], json!(fixture.instance_id));
    assert_eq!(body["mediaType"], json!("image/png"));
    assert_eq!(body["size"], json!(168));
    assert_eq!(body["encoding"], json!("base64"));
    let decoded =
        base64::engine::general_purpose::STANDARD.decode(body["data"].as_str().context("data")?)?;
    assert_eq!(decoded, bytes, "the agent must get the exact staged bytes");
    fixture.hub.shutdown().await;
    Ok(())
}

/// Scope here is narrower than the rest of the Agent surface: self only, not
/// self-and-children. An attachment belongs to one send, so a child instance
/// has no claim on its parent's images.
#[tokio::test]
async fn another_sessions_attachment_is_refused() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let object_id = stage(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &png_bytes(256),
    )
    .await?;

    let (status, _) = get_as(
        addr,
        &format!("/v1/attachments/{object_id}/content"),
        &fixture.other_agent_token,
    )
    .await?;
    assert_eq!(status, 403, "a sibling session must not read these bytes");

    // Nor can it enumerate the other session, with or without naming it.
    let (status, _) = get_as(
        addr,
        &format!("/v1/attachments?instanceId={}", fixture.instance_id),
        &fixture.other_agent_token,
    )
    .await?;
    assert_eq!(status, 403);
    let (status, body) = get_as(addr, "/v1/attachments", &fixture.other_agent_token).await?;
    assert_eq!(status, 200, "its own session still lists");
    let body = body.context("json")?;
    assert_eq!(body["instanceId"], json!(fixture.other_instance_id));
    assert_eq!(
        body["items"].as_array().context("items")?.len(),
        0,
        "nothing was staged for that session: {body}"
    );

    // Anonymous and unknown ids get nothing either.
    let (status, _, _) = raw(
        addr,
        "GET",
        &format!("/v1/attachments/{object_id}/content"),
        &[],
        None,
    )
    .await?;
    assert_eq!(status, 401);
    let (status, _) = get_as(
        addr,
        "/v1/attachments/obj_missing/content",
        &fixture.agent_token,
    )
    .await?;
    assert_eq!(status, 404);
    fixture.hub.shutdown().await;
    Ok(())
}

/// An object can be staged (5 MiB cap) and still be too large to inline
/// (3.5 MiB cap), so the refusal has to name the size and the media type
/// rather than return a truncated body.
#[tokio::test]
async fn an_attachment_over_the_inline_cap_is_refused_with_its_size_and_type() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let oversize = INLINE_CAP + 1;
    let object_id = stage(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &png_bytes(oversize),
    )
    .await?;

    let (status, body) = get_as(
        addr,
        &format!("/v1/attachments/{object_id}/content"),
        &fixture.agent_token,
    )
    .await?;
    assert_eq!(status, 400);
    let text = body.context("json")?["error"]
        .as_str()
        .context("error")?
        .to_owned();
    assert!(text.contains("RESOURCE_LIMIT"), "{text}");
    assert!(text.contains(&oversize.to_string()), "{text}");
    assert!(text.contains("image/png"), "{text}");

    // It still appears in the listing, so the agent can see why it cannot
    // have it rather than wondering where the image went.
    let (status, body) = get_as(addr, "/v1/attachments", &fixture.agent_token).await?;
    assert_eq!(status, 200);
    let body = body.context("json")?;
    assert_eq!(body["items"][0]["objectId"], json!(object_id));
    assert_eq!(body["items"][0]["size"], json!(oversize));
    fixture.hub.shutdown().await;
    Ok(())
}

/// Discovery: the listing is this session's live attachments, oldest first,
/// and an operator device must name the session it means.
#[tokio::test]
async fn listing_covers_this_session_and_operators_must_name_one() -> Result<()> {
    let fixture = fixture().await?;
    let addr = fixture.hub.addr;
    let mut staged = Vec::new();
    for index in 0..3u8 {
        let mut bytes = png_bytes(64);
        bytes.push(index);
        staged.push(
            stage(
                addr,
                &fixture.cookie,
                &fixture.instance_id,
                "image/png",
                &bytes,
            )
            .await?,
        );
    }
    // One for the other session, which must not appear below.
    stage(
        addr,
        &fixture.cookie,
        &fixture.other_instance_id,
        "image/png",
        &png_bytes(99),
    )
    .await?;

    let (status, body) = get_as(addr, "/v1/attachments", &fixture.agent_token).await?;
    assert_eq!(status, 200);
    let body = body.context("json")?;
    assert_eq!(body["instanceId"], json!(fixture.instance_id));
    let listed: Vec<&str> = body["items"]
        .as_array()
        .context("items")?
        .iter()
        .filter_map(|item| item["objectId"].as_str())
        .collect();
    assert_eq!(listed, staged, "this session's attachments, oldest first");

    // An operator has no session of its own and must say which it means.
    let (status, _, _) = raw(
        addr,
        "GET",
        "/v1/attachments",
        &[("Cookie", fixture.cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 400);
    let (status, _, body) = raw(
        addr,
        "GET",
        &format!("/v1/attachments?instanceId={}", fixture.instance_id),
        &[("Cookie", fixture.cookie.as_str())],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<Value>(&body)?["items"]
            .as_array()
            .context("items")?
            .len(),
        3
    );

    // A host token is not a device at all, so it never reaches these routes:
    // the Node keeps using D-027's `GET /v1/objects/{id}`, which is the only
    // route that resolves a host credential.
    let (status, _) = get_as(addr, "/v1/attachments", &fixture.node_token).await?;
    assert_eq!(
        status, 401,
        "a host credential authenticates on the D-027 route, not this one"
    );
    fixture.hub.shutdown().await;
    Ok(())
}

/// The read route is media-type agnostic, but D-027's staging allowlist is
/// images-only, so no `text/*` row can exist here yet. The MCP tool's text
/// branch is therefore covered in `remuda`'s own tests, against a fake Hub —
/// this asserts the gap rather than pretending to close it.
#[tokio::test]
async fn staging_is_images_only_so_no_text_row_can_reach_the_read_route() -> Result<()> {
    let mut fixture = fixture().await?;
    let addr = fixture.hub.addr;
    // Every allowlisted upload sniffs to image/*; a text body is refused.
    let (status, _, body) = raw(
        addr,
        "POST",
        &format!("/v1/objects?instanceId={}", fixture.instance_id),
        &[("Cookie", fixture.cookie.as_str())],
        Some(("text/plain", b"hello agent")),
    )
    .await?;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));

    let object_id = stage(
        addr,
        &fixture.cookie,
        &fixture.instance_id,
        "image/png",
        &png_bytes(64),
    )
    .await?;
    let (status, body) = get_as(
        addr,
        &format!("/v1/attachments/{object_id}/content"),
        &fixture.agent_token,
    )
    .await?;
    assert_eq!(status, 200);
    let body = body.context("json")?;
    assert!(
        body["mediaType"]
            .as_str()
            .is_some_and(|value| value.starts_with("image/")),
        "{body}"
    );
    assert!(body["data"].as_str().is_some_and(|data| !data.is_empty()));
    let _ = &mut fixture.node;
    fixture.hub.shutdown().await;
    Ok(())
}
