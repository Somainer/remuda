//! Provider profile CRUD: encrypted secret, GET redaction, unreachable test.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
