//! Provider profile CRUD: encrypted secret, GET redaction, unreachable test,
//! and `/discover` against a fake upstream.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let token = hub.bootstrap_token.clone();
    Ok((hub, token, dir))
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
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

/// Mint a single-use Node enroll token with a paired device's cookie (D-018).
async fn enroll_token(addr: std::net::SocketAddr, cookie: &str) -> Result<String> {
    let (status, _, rest) = http(
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

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "prov-test" }).to_string();
    let (status, head, rest) = http(addr, "POST", "/v1/login", &[], Some(&body)).await?;
    anyhow::ensure!(status == 200, "login {status} {rest}");
    cookie_from(&head).context("set-cookie")
}

#[tokio::test]
async fn provider_round_trip_encrypts_secret_and_test_is_unreachable() -> Result<()> {
    let (hub, bootstrap, dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];
    let token = "sk-test-provider-secret-zzzz";
    let create = json!({
        "name": "dummy-gateway",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "models": ["passthrough/auto"],
        "defaultModel": "passthrough/auto",
        "authToken": token,
        "defaultGateway": true
    })
    .to_string();
    let (status, _, body) = http(hub.addr, "POST", "/v1/providers", &auth, Some(&create)).await?;
    anyhow::ensure!(status == 200, "create {status} {body}");
    assert!(
        !body.contains(token),
        "GET/create leaked auth token: {body}"
    );
    let created: Value = serde_json::from_str(body.trim())?;
    assert_eq!(created["kind"], "gateway");
    assert_eq!(created["secret"]["present"], true);
    assert_eq!(created["secret"]["last4"], "zzzz");
    assert_eq!(created["secret"]["fingerprint"].as_str().unwrap().len(), 16);
    assert_eq!(created["defaultGateway"], true);
    let id = created["id"].as_str().context("id")?;

    let (status, _, body) = http(hub.addr, "GET", "/v1/providers", &auth, None).await?;
    assert_eq!(status, 200);
    assert!(!body.contains(token));
    let list: Value = serde_json::from_str(body.trim())?;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);

    let vault = dir.path().join("data/secrets/secrets.json");
    let raw = std::fs::read_to_string(&vault).context("vault")?;
    assert!(!raw.contains(token), "vault stored plaintext");
    assert!(raw.contains("nonce"));

    let (status, _, body) = http(
        hub.addr,
        "POST",
        &format!("/v1/providers/{id}/test"),
        &auth,
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "test {status} {body}");
    assert!(!body.contains(token));
    let test: Value = serde_json::from_str(body.trim())?;
    assert_eq!(test["ok"], false);
    assert_eq!(test["reachable"], false);
    let message = test["message"].as_str().unwrap_or("");
    assert!(
        message.contains("unreachable"),
        "expected unreachable message, got {message}"
    );

    let rotated = json!({ "authToken": "sk-fake-rotated-yyyy" }).to_string();
    let (status, _, body) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{id}"),
        &auth,
        Some(&rotated),
    )
    .await?;
    anyhow::ensure!(status == 200, "rotate {status} {body}");
    assert!(!body.contains("sk-fake-rotated-yyyy"));
    let patched: Value = serde_json::from_str(body.trim())?;
    assert_eq!(patched["secret"]["last4"], "yyyy");

    let (status, _, body) = http(
        hub.addr,
        "DELETE",
        &format!("/v1/providers/{id}"),
        &auth,
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "delete {status} {body}");
    let (status, _, _) = http(hub.addr, "GET", &format!("/v1/providers/{id}"), &auth, None).await?;
    assert_eq!(status, 404);
    Ok(())
}

#[tokio::test]
async fn provider_create_rejects_missing_token() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let body = json!({
        "name": "no-token",
        "kind": "gateway",
        "baseUrl": "https://gateway.example/v1"
    })
    .to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers",
        &[("Cookie", cookie.as_str())],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 400);
    assert!(rest.contains("authToken"));
    Ok(())
}

#[tokio::test]
async fn provider_scope_default_and_host_filter() -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use remuda_protocol::HostId;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    let uni = json!({
        "name": "uni-gw",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "models": ["passthrough/auto"],
        "authToken": "sk-fake-universal-zzzz",
        "defaultGateway": true,
        "scope": "universal"
    })
    .to_string();
    let (status, _, body) = http(hub.addr, "POST", "/v1/providers", &auth, Some(&uni)).await?;
    anyhow::ensure!(status == 200, "uni {status} {body}");
    let uni: Value = serde_json::from_str(body.trim())?;
    assert_eq!(uni["scope"], "universal");
    assert_eq!(uni["defaultGateway"], true);
    let uni_id = uni["id"].as_str().context("uni id")?.to_string();

    let enroll = enroll_token(hub.addr, &cookie).await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse().unwrap());
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    let host_id = HostId::new();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "h",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id.as_id().as_str(),
                "nodeVersion": "0.1.0",
                "label": "scope-host"
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let _ = {
        loop {
            let msg = node.next().await.context("hello")??;
            if let Message::Text(text) = msg {
                break serde_json::from_str::<Value>(&text)?;
            }
        }
    };
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ))
                .await;
        }
    });

    let host_scope = format!("host:{}", host_id.as_id().as_str());
    let scoped = json!({
        "name": "host-gw",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "models": ["passthrough/auto"],
        "authToken": "sk-fake-host-yyyy",
        "defaultGateway": true,
        "scope": host_scope
    })
    .to_string();
    let (status, _, body) = http(hub.addr, "POST", "/v1/providers", &auth, Some(&scoped)).await?;
    anyhow::ensure!(status == 200, "scoped {status} {body}");
    let scoped: Value = serde_json::from_str(body.trim())?;
    assert_eq!(scoped["scope"], host_scope);
    assert_eq!(scoped["defaultGateway"], true);
    let scoped_id = scoped["id"].as_str().context("scoped id")?.to_string();

    let (status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers/{uni_id}"),
        &auth,
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "{body}");
    let uni: Value = serde_json::from_str(body.trim())?;
    assert_eq!(
        uni["defaultGateway"], true,
        "universal default stays in its scope"
    );

    let (_status, _, body) = http(hub.addr, "GET", "/v1/providers", &auth, None).await?;
    let all: Value = serde_json::from_str(body.trim())?;
    assert_eq!(all["items"].as_array().map(Vec::len), Some(2));

    let (_status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers?hostId={}", host_id.as_id().as_str()),
        &auth,
        None,
    )
    .await?;
    let filtered: Value = serde_json::from_str(body.trim())?;
    let ids: Vec<&str> = filtered["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(ids.contains(&uni_id.as_str()));
    assert!(ids.contains(&scoped_id.as_str()));

    let other = HostId::new();
    let (_status, _, body) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers?hostId={}", other.as_id().as_str()),
        &auth,
        None,
    )
    .await?;
    let other_list: Value = serde_json::from_str(body.trim())?;
    let other_ids: Vec<&str> = other_list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(other_ids.contains(&uni_id.as_str()));
    assert!(!other_ids.contains(&scoped_id.as_str()));

    let bind = json!({ "providerBinding": format!("profile:{scoped_id}") }).to_string();
    let (status, _, body) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/hosts/{}", host_id.as_id().as_str()),
        &auth,
        Some(&bind),
    )
    .await?;
    anyhow::ensure!(status == 200, "bind {status} {body}");
    let host: Value = serde_json::from_str(body.trim())?;
    assert_eq!(
        host["providerBinding"],
        json!(format!("profile:{scoped_id}"))
    );

    let create = json!({
        "hostId": host_id.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-print",
        "prompt": "scoped-launch"
    })
    .to_string();
    let (status, _, body) = http(hub.addr, "POST", "/v1/instances", &auth, Some(&create)).await?;
    anyhow::ensure!(status == 200, "create {status} {body}");
    let created: Value = serde_json::from_str(body.trim())?;
    assert_eq!(created["instance"]["providerProfileId"], json!(scoped_id));
    assert_eq!(created["instance"]["providerSource"], json!("host-binding"));
    assert_eq!(
        created["instance"]["providerSourceHint"],
        json!("将使用 host-gw (host)")
    );

    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/instances",
        &auth,
        Some(
            &json!({
                "hostId": host_id.as_id().as_str(),
                "kind": "claude",
                "driver": "claude-print",
                "providerProfileId": scoped_id,
                "prompt": "explicit"
            })
            .to_string(),
        ),
    )
    .await?;
    anyhow::ensure!(status == 200, "explicit {status} {body}");
    let created: Value = serde_json::from_str(body.trim())?;
    assert_eq!(created["instance"]["providerSource"], json!("request"));
    Ok(())
}

#[tokio::test]
async fn missing_provider_defaults_reject_gateway_and_preserve_native_fallback_hint() -> Result<()>
{
    use remuda_protocol::HostId;

    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    for (label, cli) in [
        ("missing-inventory", None),
        (
            "unknown-auth",
            Some(json!([{ "kind": "claude", "auth": "unknown" }])),
        ),
        (
            "negative-auth",
            Some(json!([{
                "kind": "claude",
                "auth": "logged_out",
                "nativeGateway": false
            }])),
        ),
    ] {
        let host_id = HostId::new();
        let mut host = json!({ "maxInstances": 4 });
        if let Some(cli) = cli {
            host["cli"] = cli;
        }
        let node_task =
            connect_provider_test_node(hub.addr, &cookie, host_id.as_id().as_str(), label, host)
                .await?;

        for gateway in [
            json!({ "delegation": "gateway", "providerProfileId": "gateway" }),
            json!({ "providerProfileId": "gateway" }),
        ] {
            let mut create = gateway;
            create["hostId"] = json!(host_id.as_id().as_str());
            create["kind"] = json!("claude");
            create["driver"] = json!("claude-print");
            let (status, _, body) = http(
                hub.addr,
                "POST",
                "/v1/instances",
                &auth,
                Some(&create.to_string()),
            )
            .await?;
            assert_eq!(status, 422, "{label}: {body}");
            let rejected: Value = serde_json::from_str(body.trim())?;
            assert_eq!(rejected["code"], json!("PROVIDER_NOT_CONFIGURED"));
            assert_eq!(
                rejected["reasons"],
                json!([format!(
                    "no gateway provider configured for host {}; add a provider or choose native",
                    host_id.as_id().as_str()
                )])
            );
        }
        let (status, _, body) = http(
            hub.addr,
            "GET",
            &format!("/v1/instances?hostId={}", host_id.as_id().as_str()),
            &auth,
            None,
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        let instances: Value = serde_json::from_str(body.trim())?;
        assert_eq!(
            instances["items"],
            json!([]),
            "rejected gateway created an instance"
        );

        for delegation in [None, Some("direct")] {
            let mut create = json!({
                "hostId": host_id.as_id().as_str(),
                "kind": "claude",
                "driver": "claude-print",
                "prompt": "native-fallback"
            });
            if let Some(delegation) = delegation {
                create["delegation"] = json!(delegation);
            }
            let (status, _, body) = http(
                hub.addr,
                "POST",
                "/v1/instances",
                &auth,
                Some(&create.to_string()),
            )
            .await?;
            anyhow::ensure!(
                status == 200,
                "{label}, delegation {delegation:?}: create {status} {body}"
            );
            let created: Value = serde_json::from_str(body.trim())?;
            let instance = &created["instance"];
            assert_eq!(instance["providerSource"], json!("native-fallback"));
            assert_eq!(instance["providerProfileId"], json!("none"));
            assert_eq!(instance["delegation"], json!("none"));
            let hint = instance["providerSourceHint"]
                .as_str()
                .context("native fallback hint")?;
            assert!(hint.contains("未匹配到默认供应商配置"), "{hint}");
            assert!(hint.contains("尝试使用主机原生认证"), "{hint}");
            let instance_id = instance["instanceId"].as_str().context("instanceId")?;
            let (status, _, body) = http(
                hub.addr,
                "GET",
                &format!("/v1/instances/{instance_id}"),
                &auth,
                None,
            )
            .await?;
            anyhow::ensure!(status == 200, "get {status} {body}");
            let persisted: Value = serde_json::from_str(body.trim())?;
            assert_eq!(persisted["providerSource"], instance["providerSource"]);
            assert_eq!(
                persisted["providerSourceHint"],
                instance["providerSourceHint"]
            );
            assert_eq!(persisted["providerProfileId"], json!("none"));
            assert_eq!(persisted["delegation"], json!("none"));
        }
        node_task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn explicit_gateway_placement_skips_hosts_without_a_default_provider() -> Result<()> {
    use remuda_protocol::HostId;

    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];
    let mut host_ids = [
        HostId::new().as_id().as_str().to_string(),
        HostId::new().as_id().as_str().to_string(),
    ];
    host_ids.sort();
    let [missing_host_id, configured_host_id] = host_ids;
    let host = json!({ "maxInstances": 4, "labels": { "region": "test" } });
    let missing_node = connect_provider_test_node(
        hub.addr,
        &cookie,
        &missing_host_id,
        "no-default-provider",
        host.clone(),
    )
    .await?;
    let configured_node = connect_provider_test_node(
        hub.addr,
        &cookie,
        &configured_host_id,
        "configured-provider",
        host,
    )
    .await?;
    let provider = json!({
        "name": "host-gw",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "authToken": "sk-fake-host-yyyy",
        "defaultGateway": true,
        "scope": format!("host:{configured_host_id}")
    });
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/providers",
        &auth,
        Some(&provider.to_string()),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let provider: Value = serde_json::from_str(body.trim())?;

    for placement in [
        json!({ "kind": "any" }),
        json!({ "kind": "labels", "labels": ["region=test"] }),
    ] {
        let create = json!({
            "placement": placement,
            "kind": "claude",
            "driver": "claude-print",
            "delegation": "gateway",
            "providerProfileId": "gateway"
        });
        let (status, _, body) = http(
            hub.addr,
            "POST",
            "/v1/placement/resolve",
            &auth,
            Some(&create.to_string()),
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        let ranked: Value = serde_json::from_str(body.trim())?;
        assert_eq!(ranked["hostId"], json!(missing_host_id));
        assert_eq!(ranked["hosts"].as_array().map(Vec::len), Some(2));

        let (status, _, body) = http(
            hub.addr,
            "POST",
            "/v1/instances",
            &auth,
            Some(&create.to_string()),
        )
        .await?;
        assert_eq!(status, 200, "{placement}: {body}");
        let created: Value = serde_json::from_str(body.trim())?;
        assert_eq!(created["hostId"], json!(configured_host_id));
        assert_eq!(created["instance"]["providerProfileId"], provider["id"]);
        assert_eq!(
            created["instance"]["providerSource"],
            json!("host-scoped-default")
        );
    }
    missing_node.abort();
    configured_node.abort();
    Ok(())
}

async fn connect_provider_test_node(
    addr: std::net::SocketAddr,
    cookie: &str,
    host_id: &str,
    label: &str,
    host: Value,
) -> Result<tokio::task::JoinHandle<()>> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let enroll = enroll_token(addr, cookie).await?;
    let mut req = format!("ws://{addr}/v1/node").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": label,
                "host": host
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    loop {
        let message = node.next().await.context("hello")??;
        if let Message::Text(text) = message {
            let hello: Value = serde_json::from_str(&text)?;
            anyhow::ensure!(hello["result"]["hostId"] == host_id, "hello {hello}");
            break;
        }
    }
    Ok(tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame.get("method").is_none() {
                continue;
            }
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = node
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
                        .to_string()
                        .into(),
                ))
                .await;
        }
    }))
}

/// A fake gateway serving `/v1/models`. Returns its address and the tokens it
/// was presented with, so a test can assert the probe forwarded the secret
/// without the Hub ever echoing it back to the caller.
struct FakeUpstream {
    addr: std::net::SocketAddr,
    seen: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    /// Serve `body` for every `/v1/models` GET; anything else is a 404.
    async fn serve(body: &'static str) -> Result<Self> {
        Self::serve_with_status(200, body).await
    }

    async fn serve_with_status(status: u16, body: &'static str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    for line in head.lines() {
                        // reqwest emits lowercase header names.
                        if let Some(value) = line
                            .split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                            .map(|(_, value)| value)
                        {
                            recorded.lock().await.push(value.trim().to_string());
                        }
                    }
                    let (code, payload) = if head.starts_with("GET /v1/models") {
                        (status, body)
                    } else {
                        (404, "{}")
                    };
                    let response = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Ok(Self { addr, seen, task })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const ANTHROPIC_MODELS: &str = r#"{"data":[
    {"type":"model","id":"gw/fable","display_name":"Fable","created_at":"2026-01-01"},
    {"type":"model","id":"gw/wide","display_name":"Wide","context_window":1048576}
],"has_more":false}"#;

const OPENAI_MODELS: &str = r#"{"object":"list","data":[
    {"id":"gw/small","object":"model","owned_by":"gw"},
    {"id":"gw/large","object":"model","context_length":200000}
]}"#;

#[tokio::test]
async fn discover_normalizes_both_upstream_shapes_without_echoing_the_token() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    let anthropic = FakeUpstream::serve(ANTHROPIC_MODELS).await?;
    let token = "sk-fake-discover-wwww";
    let body = json!({ "baseUrl": anthropic.base_url(), "token": token }).to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "discover {status} {rest}");
    assert!(!rest.contains(token), "discover echoed the token: {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(result["ok"], true);
    assert_eq!(result["reachable"], true);
    assert_eq!(result["status"], 200);
    let models = result["models"].as_array().context("models")?;
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "gw/fable");
    assert_eq!(models[0]["label"], "Fable");
    assert_eq!(models[0]["enabled"], true);
    assert_eq!(models[1]["contextWindow"], 1_048_576);
    assert_eq!(models[1]["tags"], json!(["1m"]));
    // The probe authenticated with the supplied token even though the profile
    // does not exist yet.
    assert_eq!(
        anthropic.seen.lock().await.first().map(String::as_str),
        Some(format!("Bearer {token}").as_str())
    );

    let openai = FakeUpstream::serve(OPENAI_MODELS).await?;
    let body = json!({ "baseUrl": openai.base_url() }).to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "openai discover {status} {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    let ids: Vec<&str> = result["models"]
        .as_array()
        .context("models")?
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["gw/small", "gw/large"]);
    assert_eq!(result["models"][1]["contextWindow"], 200_000);
    assert_eq!(result["models"][1]["tags"], json!([]));
    Ok(())
}

/// A gateway that answers `/v1/models` differently depending on the headers
/// (astergate: a broad OpenAI-style list for a plain Bearer GET, a short
/// `claude-*` subset once `anthropic-version` is set). Probing one surface
/// under-reports, so `/discover` and `/test` must union both.
struct PerHeaderUpstream {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl PerHeaderUpstream {
    async fn serve() -> Result<Self> {
        const OPENAI: &str = r#"{"object":"list","data":[
            {"id":"passthrough/ark/seed-evolving"},
            {"id":"cursor/gpt-5"},
            {"id":"claude-opus-5"}
        ]}"#;
        const ANTHROPIC: &str = r#"{"data":[
            {"type":"model","id":"claude-opus-5","display_name":"Opus 5","context_window":1048576},
            {"type":"model","id":"claude-haiku-4-5","display_name":"Haiku 4.5"}
        ],"has_more":false}"#;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    let (code, payload) = if head.starts_with("get /v1/models") {
                        if head.contains("anthropic-version:") {
                            (200, ANTHROPIC)
                        } else {
                            (200, OPENAI)
                        }
                    } else {
                        (404, "{}")
                    };
                    let response = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Ok(Self { addr, task })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for PerHeaderUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn discover_unions_the_two_catalogs_a_gateway_serves_per_header() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    let upstream = PerHeaderUpstream::serve().await?;
    let token = "sk-fake-per-header-zzzz";
    let body = json!({ "baseUrl": upstream.base_url(), "token": token }).to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "discover {status} {rest}");
    assert!(!rest.contains(token), "discover echoed the token: {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(result["ok"], true);
    let models = result["models"].as_array().context("models")?;
    let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
    // Every id from either listing, nothing filtered by prefix.
    assert_eq!(
        ids,
        vec![
            "passthrough/ark/seed-evolving",
            "cursor/gpt-5",
            "claude-opus-5",
            "claude-haiku-4-5",
        ]
    );
    // The overlapping id records both surfaces and keeps the richer metadata
    // that only the anthropic listing reported.
    let shared = models.iter().find(|m| m["id"] == "claude-opus-5").unwrap();
    assert_eq!(shared["surfaces"], json!(["openai", "anthropic"]));
    assert_eq!(shared["label"], "Opus 5");
    assert_eq!(shared["contextWindow"], 1_048_576);
    assert_eq!(
        models[0]["surfaces"],
        json!(["openai"]),
        "an id only the plain listing serves is tagged openai"
    );
    assert_eq!(
        models[3]["surfaces"],
        json!(["anthropic"]),
        "an id only the anthropic listing serves is still offered"
    );
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("openai+anthropic"),
        "message names both surfaces: {}",
        result["message"]
    );
    Ok(())
}

#[tokio::test]
async fn discover_reuses_a_saved_profile_token_and_test_returns_the_same_shape() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];
    let upstream = FakeUpstream::serve(ANTHROPIC_MODELS).await?;
    let token = "sk-fake-saved-vvvv";
    let create = json!({
        "name": "fake-upstream",
        "kind": "gateway",
        "baseUrl": upstream.base_url(),
        "authToken": token,
    })
    .to_string();
    let (status, _, rest) = http(hub.addr, "POST", "/v1/providers", &auth, Some(&create)).await?;
    anyhow::ensure!(status == 200, "create {status} {rest}");
    let created: Value = serde_json::from_str(rest.trim())?;
    let id = created["id"].as_str().context("id")?;

    // No token in the body: the Hub loads the stored one.
    let body = json!({ "profileId": id }).to_string();
    let (status, _, discovered) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&body),
    )
    .await?;
    anyhow::ensure!(status == 200, "discover {status} {discovered}");
    assert!(!discovered.contains(token));
    assert_eq!(
        upstream.seen.lock().await.first().map(String::as_str),
        Some(format!("Bearer {token}").as_str())
    );

    let (status, _, tested) = http(
        hub.addr,
        "POST",
        &format!("/v1/providers/{id}/test"),
        &auth,
        Some("{}"),
    )
    .await?;
    anyhow::ensure!(status == 200, "test {status} {tested}");
    assert!(!tested.contains(token));
    let discovered: Value = serde_json::from_str(discovered.trim())?;
    let tested: Value = serde_json::from_str(tested.trim())?;
    assert_eq!(
        discovered["models"], tested["models"],
        "/test and /discover must report the same normalized catalog"
    );
    assert_eq!(discovered["ok"], tested["ok"]);
    Ok(())
}

#[tokio::test]
async fn discover_requires_auth_a_base_url_and_reports_unreachable() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    // No device cookie at all.
    let body = json!({ "baseUrl": "http://127.0.0.1:1" }).to_string();
    let (status, _, _) = http(hub.addr, "POST", "/v1/providers/discover", &[], Some(&body)).await?;
    assert_eq!(status, 401, "discover must require a device");

    // Missing / malformed base URL.
    for bad in [json!({}), json!({ "baseUrl": "ftp://nope" })] {
        let (status, _, rest) = http(
            hub.addr,
            "POST",
            "/v1/providers/discover",
            &auth,
            Some(&bad.to_string()),
        )
        .await?;
        assert_eq!(status, 400, "{bad}: {rest}");
    }

    // The token must not travel in `headers`.
    let leaky = json!({
        "baseUrl": "http://127.0.0.1:1",
        "headers": { "Authorization": "Bearer sk-leak" }
    })
    .to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&leaky),
    )
    .await?;
    assert_eq!(status, 400, "{rest}");

    // Unknown profile.
    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&json!({ "profileId": "pvp_missing" }).to_string()),
    )
    .await?;
    assert_eq!(status, 404);

    // Reachability failures mirror `/test`.
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&json!({ "baseUrl": "http://127.0.0.1:1" }).to_string()),
    )
    .await?;
    anyhow::ensure!(status == 200, "unreachable {status} {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(result["ok"], false);
    assert_eq!(result["reachable"], false);
    assert_eq!(result["models"], json!([]));
    assert!(
        result["message"]
            .as_str()
            .unwrap_or("")
            .contains("unreachable"),
        "{result}"
    );

    // An upstream that answers but rejects the credential is reachable, not ok.
    let denied = FakeUpstream::serve_with_status(401, r#"{"error":"no"}"#).await?;
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers/discover",
        &auth,
        Some(&json!({ "baseUrl": denied.base_url() }).to_string()),
    )
    .await?;
    anyhow::ensure!(status == 200, "401 upstream {status} {rest}");
    let result: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(result["ok"], false);
    assert_eq!(result["reachable"], true);
    assert_eq!(result["models"], json!([]));
    assert!(
        result["message"]
            .as_str()
            .unwrap_or("")
            .contains("auth failed"),
        "{result}"
    );
    Ok(())
}

#[tokio::test]
async fn structured_models_round_trip_and_default_model_must_be_enabled() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];
    let create = json!({
        "name": "structured",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "models": [
            { "id": "gw/on", "enabled": true, "label": "On", "contextWindow": 1048576, "tags": ["1m"] },
            { "id": "gw/off", "enabled": false }
        ],
        "defaultModel": "gw/on",
        "authToken": "sk-structured-xxxx"
    })
    .to_string();
    let (status, _, rest) = http(hub.addr, "POST", "/v1/providers", &auth, Some(&create)).await?;
    anyhow::ensure!(status == 200, "create {status} {rest}");
    let created: Value = serde_json::from_str(rest.trim())?;
    let id = created["id"].as_str().context("id")?.to_string();
    assert_eq!(created["models"][0]["label"], "On");
    assert_eq!(created["models"][0]["contextWindow"], 1_048_576);
    assert_eq!(created["models"][0]["tags"], json!(["1m"]));
    assert_eq!(created["models"][1]["enabled"], false);
    assert_eq!(created["defaultModel"], "gw/on");

    // A disabled model cannot be the default.
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/providers",
        &auth,
        Some(
            &json!({
                "name": "bad-default",
                "kind": "gateway",
                "baseUrl": "http://127.0.0.1:1",
                "models": [{ "id": "gw/on" }, { "id": "gw/off", "enabled": false }],
                "defaultModel": "gw/off",
                "authToken": "sk-bad-default-xxxx"
            })
            .to_string(),
        ),
    )
    .await?;
    assert_eq!(status, 400, "{rest}");
    assert!(rest.contains("defaultModel"), "{rest}");

    // Patching the catalog so the saved default disappears re-resolves it to
    // the first enabled model rather than leaving an orphan.
    let patch = json!({ "models": [{ "id": "gw/new" }, { "id": "gw/other" }] }).to_string();
    let (status, _, rest) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{id}"),
        &auth,
        Some(&patch),
    )
    .await?;
    anyhow::ensure!(status == 200, "patch {status} {rest}");
    let patched: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(patched["defaultModel"], "gw/new");

    // Disabling every model leaves no default to prefill.
    let patch = json!({ "models": [{ "id": "gw/new", "enabled": false }] }).to_string();
    let (status, _, rest) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{id}"),
        &auth,
        Some(&patch),
    )
    .await?;
    anyhow::ensure!(status == 200, "patch {status} {rest}");
    let patched: Value = serde_json::from_str(rest.trim())?;
    assert_eq!(patched["defaultModel"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn a_legacy_string_catalog_migrates_to_structured_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    let id = "pvp_00000000-0000-7000-8000-00000000legacy";

    // Boot once so the schema exists, create a row, then rewrite its catalog to
    // the pre-migration string form directly in SQLite.
    {
        let hub = spawn(HubConfig::for_test(data.clone())).await?;
        let cookie = login(hub.addr, &hub.bootstrap_token.clone()).await?;
        let create = json!({
            "name": "legacy",
            "kind": "gateway",
            "baseUrl": "http://127.0.0.1:1",
            "models": ["passthrough/auto"],
            "authToken": "sk-fake-legacy-llll"
        })
        .to_string();
        let (status, _, rest) = http(
            hub.addr,
            "POST",
            "/v1/providers",
            &[("Cookie", cookie.as_str())],
            Some(&create),
        )
        .await?;
        anyhow::ensure!(status == 200, "create {status} {rest}");
        drop(hub);
    }
    let db = data.join("hub.sqlite");
    let conn = rusqlite::Connection::open(&db)?;
    conn.execute(
        "UPDATE provider_profiles SET id = ?1, models_json = ?2",
        rusqlite::params![id, r#"["passthrough/auto","passthrough/auto_model"]"#],
    )?;
    drop(conn);

    // Reopening runs the migration.
    let hub = spawn(HubConfig::for_test(data.clone())).await?;
    let cookie = login(hub.addr, &hub.bootstrap_token.clone()).await?;
    let (status, _, rest) = http(
        hub.addr,
        "GET",
        &format!("/v1/providers/{id}"),
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await?;
    anyhow::ensure!(status == 200, "get {status} {rest}");
    let profile: Value = serde_json::from_str(rest.trim())?;
    let models = profile["models"].as_array().context("models")?;
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "passthrough/auto");
    assert_eq!(models[0]["enabled"], true, "migrated models stay usable");
    assert_eq!(models[1]["id"], "passthrough/auto_model");
    assert_eq!(models[1]["enabled"], true);
    drop(hub);

    // The stored column is rewritten, not just parsed on read.
    let conn = rusqlite::Connection::open(&db)?;
    let stored: String = conn.query_row(
        "SELECT models_json FROM provider_profiles WHERE id = ?1",
        rusqlite::params![id],
        |row| row.get(0),
    )?;
    let stored: Value = serde_json::from_str(&stored)?;
    assert!(stored[0].is_object(), "still a bare string list: {stored}");
    assert_eq!(stored[0]["id"], "passthrough/auto");
    assert_eq!(stored[0]["enabled"], true);
    Ok(())
}

/// D-047: the delivery fields are accepted and *applied* (api-routing task 2).
///
/// A create/PATCH stores the profile delivery and reads it back; a body
/// carrying `apiVia`/`apiRoute` is parsed (a typo is a 400, never a silent
/// direct launch), and the malformed `via` shape is rejected at write time.
#[tokio::test]
async fn delivery_fields_are_stored_and_applied() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let auth = [("Cookie", cookie.as_str())];

    // A create body carrying a full via delivery.
    let body = json!({
        "name": "routed-gw",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "models": ["passthrough/auto"],
        "authToken": "sk-fake-routed-zzzz",
        "delivery": {
            "mode": "via",
            "viaHostId": "hst_01993ab0-0000-7000-8000-000000000007",
            "route": "hub-relay"
        }
    })
    .to_string();
    let (status, _, created) = http(hub.addr, "POST", "/v1/providers", &auth, Some(&body)).await?;
    anyhow::ensure!(status == 200, "create {status} {created}");
    let created: Value = serde_json::from_str(created.trim())?;
    assert_eq!(
        created["delivery"],
        json!({
            "mode": "via",
            "viaHostId": "hst_01993ab0-0000-7000-8000-000000000007",
            "route": "hub-relay"
        }),
        "the applied delivery reads back verbatim: {created}"
    );
    let id = created["id"].as_str().context("id")?.to_string();

    // A PATCH replaces the delivery; route omitted on a write defaults to auto.
    let patch = json!({ "delivery": { "mode": "via", "viaHostId": "hst_01993ab0-0000-7000-8000-000000000007" } })
        .to_string();
    let (status, _, patched) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{id}"),
        &auth,
        Some(&patch),
    )
    .await?;
    anyhow::ensure!(status == 200, "patch {status} {patched}");
    let patched: Value = serde_json::from_str(patched.trim())?;
    assert_eq!(patched["delivery"]["route"], json!("auto"), "{patched}");

    // Back to direct.
    let direct = json!({ "delivery": { "mode": "direct" } }).to_string();
    let (status, _, patched) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{id}"),
        &auth,
        Some(&direct),
    )
    .await?;
    anyhow::ensure!(status == 200, "direct patch {status} {patched}");
    let patched: Value = serde_json::from_str(patched.trim())?;
    assert!(patched["delivery"].get("viaHostId").is_none(), "{patched}");

    // `via` without a host is a parse-level rejection (400), not stored.
    let bad = json!({ "delivery": { "mode": "via", "route": "hub-relay" } }).to_string();
    let (status, _, rejected) = http(
        hub.addr,
        "PATCH",
        &format!("/v1/providers/{id}"),
        &auth,
        Some(&bad),
    )
    .await?;
    assert!(
        status == 400 || status == 422,
        "via without viaHostId must be a client error: {status} {rejected}"
    );

    // The dispatch surface parses the same two fields; no such project is the
    // failure, not a body-shape rejection.
    let dispatch = json!({
        "projectId": "prj_01993ab0-0000-7000-8000-000000000001",
        "brief": "noop",
        "apiVia": "none"
    })
    .to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/workers/dispatch",
        &auth,
        Some(&dispatch),
    )
    .await?;
    assert_ne!(status, 200, "no such project should not dispatch");
    for unknown in ["unknown field", "unknown variant"] {
        assert!(
            !rest.contains(unknown),
            "the delivery fields must not be a body-parse rejection: {status} {rest}"
        );
    }

    // An unknown apiVia spelling is a 400 rather than a silent direct launch.
    let bad_via = json!({
        "projectId": "prj_01993ab0-0000-7000-8000-000000000001",
        "brief": "noop",
        "apiVia": "hst-not-an-id"
    })
    .to_string();
    let (status, _, rest) = http(
        hub.addr,
        "POST",
        "/v1/workers/dispatch",
        &auth,
        Some(&bad_via),
    )
    .await?;
    assert_eq!(status, 400, "a malformed apiVia is a 400: {rest}");

    // apiRoute without apiVia is a 400 too.
    let bare_route = json!({
        "projectId": "prj_01993ab0-0000-7000-8000-000000000001",
        "brief": "noop",
        "apiRoute": "hub-relay"
    })
    .to_string();
    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/workers/dispatch",
        &auth,
        Some(&bare_route),
    )
    .await?;
    assert_eq!(status, 400);
    Ok(())
}
