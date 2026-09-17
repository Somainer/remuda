//! Read-only host files routes: operator proxy, Agent refusal, host binding,
//! and the Node-facing staging endpoint. Reuses the objects test harness.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(bytes) = body {
        head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(bytes) = body {
        request.extend_from_slice(bytes);
    }
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("no header terminator"))?;
    let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Ok((status, buf[split + 4..].to_vec()))
}

async fn json_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String)> {
    let mut heads = headers.to_vec();
    let bytes = body.map(|text| {
        heads.push(("Content-Type", "application/json"));
        text.as_bytes()
    });
    let (status, payload) = raw(addr, method, path, &heads, bytes).await?;
    Ok((status, String::from_utf8_lossy(&payload).to_string()))
}

fn cookie_from(head: String) -> Option<String> {
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

/// Login leaves the Set-Cookie value in `raw`'s discarded header, so do this
/// one with a minimal dedicated request that returns the head.
async fn login(addr: std::net::SocketAddr, bootstrap: &str, name: &str) -> Result<String> {
    let body = json!({"bootstrapToken": bootstrap, "deviceName": name}).to_string();
    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!(
                "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("header terminator")?;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    anyhow::ensure!(
        head.lines().next().unwrap_or_default().contains(" 200 "),
        "{head}"
    );
    cookie_from(head).context("set-cookie")
}

struct Fixture {
    hub: remuda_hub::RunningHub,
    addr: std::net::SocketAddr,
    cookie: String,
    host_id: String,
    node_token: String,
    _socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
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

async fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap, "host-files-phone").await?;

    // Mint + use an enroll token exactly like the objects harness.
    let (_, rest) = json_request(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", &cookie)],
        Some("{}"),
    )
    .await?;
    let enroll = serde_json::from_str::<Value>(rest.trim())?["token"]
        .as_str()
        .context("enroll token")?
        .to_owned();

    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut socket, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    let host_id = HostId::new().as_id().as_str().to_owned();
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params":{"hostId": host_id, "nodeVersion":"0.1.0", "label":"host-files-node"}})
            .to_string()
            .into(),
        ))
        .await?;
    let hello = recv_json(&mut socket).await?;
    let node_token = hello
        .pointer("/result/nodeToken")
        .and_then(Value::as_str)
        .context("node token")?
        .to_owned();
    Ok(Fixture {
        hub,
        addr,
        cookie,
        host_id,
        node_token,
        _socket: socket,
    })
}

#[tokio::test]
async fn human_operator_gets_list_and_read_results() -> Result<()> {
    let fixture = fixture().await?;
    let (addr, cookie, host) = (
        fixture.addr,
        fixture.cookie.clone(),
        fixture.host_id.clone(),
    );

    // Script the Node's list/read answers through the live transport.
    fixture
        .hub
        .test_set_node_reply(
            &host,
            Some(json!({"result": {"workspaceId": "wsp_x", "path": "/ws",
                "entries": [{"name": "notes.txt", "kind": "file", "size": 17,
                             "mtime": 0, "mode": 420}]}})),
        )
        .await;
    let (status, body) = json_request(
        addr,
        "GET",
        &format!("/v1/hosts/{host}/files?workspaceId=wsp_x"),
        &[("Cookie", &cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let listing = serde_json::from_str::<Value>(body.trim())?;
    assert_eq!(listing["entries"][0]["name"], json!("notes.txt"));

    fixture
        .hub
        .test_set_node_reply(
            &host,
            Some(json!({"result": {"objectId": "obj_staged", "digest": "0", "size": 17}})),
        )
        .await;
    let (status, body) = json_request(
        addr,
        "POST",
        &format!("/v1/hosts/{host}/files/read"),
        &[("Cookie", &cookie)],
        Some(r#"{"workspaceId":"wsp_x","relPath":"notes.txt"}"#),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let read = serde_json::from_str::<Value>(body.trim())?;
    assert_eq!(read["objectId"], json!("obj_staged"));
    assert_eq!(read["size"], json!(17));

    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn agent_origin_is_always_403() -> Result<()> {
    let fixture = fixture().await?;
    let agent = fixture
        .hub
        .test_mint_agent_token("host-files-agent", "ins_unrelated")
        .await?;
    let (status, _) = json_request(
        fixture.addr,
        "GET",
        &format!("/v1/hosts/{}/files?workspaceId=wsp_x", fixture.host_id),
        &[("Authorization", &format!("Bearer {agent}"))],
        None,
    )
    .await?;
    assert_eq!(status, 403);
    let (status, _) = json_request(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{}/files/read", fixture.host_id),
        &[("Authorization", &format!("Bearer {agent}"))],
        Some(r#"{"workspaceId":"wsp_x"}"#),
    )
    .await?;
    assert_eq!(status, 403);
    let (status, _) = json_request(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{}/files/search", fixture.host_id),
        &[("Authorization", &format!("Bearer {agent}"))],
        Some(r#"{"workspaceId":"wsp_x","query":"needle","mode":"content"}"#),
    )
    .await?;
    assert_eq!(status, 403);
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn human_operator_gets_search_results() -> Result<()> {
    let fixture = fixture().await?;
    let (addr, cookie, host) = (
        fixture.addr,
        fixture.cookie.clone(),
        fixture.host_id.clone(),
    );

    fixture
        .hub
        .test_set_node_reply(
            &host,
            Some(json!({"result": {
                "workspaceId": "wsp_x",
                "path": "/ws",
                "mode": "content",
                "query": "needle",
                "matches": [
                    {"path": "src/main.rs", "line": 3, "column": 4,
                     "snippet": "let needle = 1;"}
                ],
                "truncated": false,
                "filesScanned": 1,
                "bytesScanned": 17
            }})),
        )
        .await;
    let (status, body) = json_request(
        addr,
        "POST",
        &format!("/v1/hosts/{host}/files/search"),
        &[("Cookie", &cookie)],
        Some(
            r#"{"workspaceId":"wsp_x","query":"needle","mode":"content",
                 "regex":false,"glob":"*.rs","maxResults":10}"#,
        ),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let result = serde_json::from_str::<Value>(body.trim())?;
    assert_eq!(result["matches"][0]["path"], json!("src/main.rs"));
    assert_eq!(result["matches"][0]["line"], json!(3));
    assert_eq!(result["truncated"], json!(false));

    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn search_rejects_bad_query_and_unknown_workspace() -> Result<()> {
    let fixture = fixture().await?;
    let (addr, cookie, host) = (
        fixture.addr,
        fixture.cookie.clone(),
        fixture.host_id.clone(),
    );

    // Empty query is refused at the Hub before the Node is called.
    let (status, body) = json_request(
        addr,
        "POST",
        &format!("/v1/hosts/{host}/files/search"),
        &[("Cookie", &cookie)],
        Some(r#"{"workspaceId":"wsp_x","query":"   "}"#),
    )
    .await?;
    assert_eq!(status, 400, "{body}");

    // A bad mode is likewise a Hub-side 400.
    let (status, _) = json_request(
        addr,
        "POST",
        &format!("/v1/hosts/{host}/files/search"),
        &[("Cookie", &cookie)],
        Some(r#"{"workspaceId":"wsp_x","query":"x","mode":"contents"}"#),
    )
    .await?;
    assert_eq!(status, 400);

    // The Node's "unknown workspace" JSON-RPC error surfaces as a clean 400,
    // never a hang or a 5xx.
    fixture
        .hub
        .test_set_node_reply(
            &host,
            Some(json!({"error": {"code": -32602,
                "message": "workspace wsp_missing is not registered on this Node"}})),
        )
        .await;
    let (status, body) = json_request(
        addr,
        "POST",
        &format!("/v1/hosts/{host}/files/search"),
        &[("Cookie", &cookie)],
        Some(r#"{"workspaceId":"wsp_missing","query":"needle"}"#),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("not registered"), "{body}");

    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_and_offline_hosts_are_rejected() -> Result<()> {
    let fixture = fixture().await?;
    // Unknown host → 404.
    let ghost = HostId::new().as_id().as_str().to_owned();
    let (status, _) = json_request(
        fixture.addr,
        "GET",
        &format!("/v1/hosts/{ghost}/files?workspaceId=wsp_x"),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 404);
    let (status, _) = json_request(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{ghost}/files/search"),
        &[("Cookie", &fixture.cookie)],
        Some(r#"{"workspaceId":"wsp_x","query":"needle"}"#),
    )
    .await?;
    assert_eq!(status, 404);
    // A registered but disconnected host → 409 host offline.
    fixture.hub.test_disconnect_node(&fixture.host_id).await;
    let (status, body) = json_request(
        fixture.addr,
        "GET",
        &format!("/v1/hosts/{}/files?workspaceId=wsp_x", fixture.host_id),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 409, "{body}");
    let (status, body) = json_request(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{}/files/search", fixture.host_id),
        &[("Cookie", &fixture.cookie)],
        Some(r#"{"workspaceId":"wsp_x","query":"needle"}"#),
    )
    .await?;
    assert_eq!(status, 409, "{body}");
    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn node_stages_with_its_own_token_and_another_host_cannot() -> Result<()> {
    let fixture = fixture().await?;
    let bytes = b"read off the workstation\n".to_vec();

    // The owning Node stages bytes under its own host.
    let (status, body) = raw(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{}/files/objects?name=notes.txt", fixture.host_id),
        &[
            ("Authorization", &format!("Bearer {}", fixture.node_token)),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(&bytes),
    )
    .await?;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let staged = serde_json::from_str::<Value>(String::from_utf8_lossy(&body).trim())?;
    let object_id = staged["objectId"].as_str().context("objectId")?.to_owned();
    assert_eq!(staged["size"], json!(bytes.len()));

    // The same token reads the bytes back through the existing objects route.
    let (status, pulled) = raw(
        fixture.addr,
        "GET",
        &format!("/v1/objects/{object_id}"),
        &[("Authorization", &format!("Bearer {}", fixture.node_token))],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(pulled, bytes);

    // A second Node's host token is refused on the first host's stage route.
    let other_cookie = fixture.cookie.clone();
    let other = fixture_other_node(&fixture.hub, other_cookie).await?;
    let (status, _) = raw(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{}/files/objects", fixture.host_id),
        &[
            ("Authorization", &format!("Bearer {other}")),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(b"x"),
    )
    .await?;
    assert_eq!(status, 403, "a host token must not stage on another host");

    // Anonymous staging is refused.
    let (status, _) = raw(
        fixture.addr,
        "POST",
        &format!("/v1/hosts/{}/files/objects", fixture.host_id),
        &[("Content-Type", "application/octet-stream")],
        Some(b"x"),
    )
    .await?;
    assert_eq!(status, 401);

    // Human operator can download the staged object through the normal route.
    let (status, pulled) = raw(
        fixture.addr,
        "GET",
        &format!("/v1/objects/{object_id}"),
        &[("Cookie", &fixture.cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(pulled, bytes);

    fixture.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn staging_rejects_oversize_and_unsanitized_names() -> Result<()> {
    let fixture = fixture().await?;
    // Declared length over the 25 MiB default cap is rejected on the header
    // before any bytes are streamed.
    let mut stream = TcpStream::connect(fixture.addr).await?;
    let head = format!(
        "POST /v1/hosts/{}/files/objects HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\
         Authorization: Bearer {}\r\n\r\n",
        fixture.host_id,
        fixture.addr,
        25 * 1024 * 1024 + 1,
        fixture.node_token
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(b"x").await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head_text = String::from_utf8_lossy(&buf[..split]).to_string();
    assert!(head_text.contains(" 413 "), "{head_text}");
    assert!(String::from_utf8_lossy(&buf[split + 4..]).contains("RESOURCE_LIMIT"));

    // A traversal-shaped name is refused rather than stored.
    let (status, _) = raw(
        fixture.addr,
        "POST",
        &format!(
            "/v1/hosts/{}/files/objects?name=..%2Fescape",
            fixture.host_id
        ),
        &[
            ("Authorization", &format!("Bearer {}", fixture.node_token)),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(b"x"),
    )
    .await?;
    assert_eq!(status, 400);
    fixture.hub.shutdown().await;
    Ok(())
}

/// Enroll a second Node against the same Hub and return its durable token.
async fn fixture_other_node(hub: &remuda_hub::RunningHub, cookie: String) -> Result<String> {
    let addr = hub.addr;
    let (_, token_json) = json_request(
        addr,
        "POST",
        "/v1/hosts/enroll-token",
        &[("Cookie", &cookie)],
        Some("{}"),
    )
    .await?;
    let enroll = serde_json::from_str::<Value>(token_json.trim())?["token"]
        .as_str()
        .context("enroll token")?
        .to_owned();
    let mut request = format!("ws://{addr}/v1/node").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut socket, _) =
        tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params":{"hostId": HostId::new().as_id().as_str(), "nodeVersion":"0.1.0"}})
            .to_string()
            .into(),
        ))
        .await?;
    let hello = recv_json(&mut socket).await?;
    // The socket must stay open for the host's link to remain enrolled.
    std::mem::forget(socket);
    hello
        .pointer("/result/nodeToken")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("other node token")
}
