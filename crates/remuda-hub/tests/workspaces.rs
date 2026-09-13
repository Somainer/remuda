//! D-023 routes: two-phase settlement, durable host projection, and operator scope.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[tokio::test]
async fn workspace_routes_require_settlement_persist_and_forbid_agents() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("hub");
    let hub = spawn(HubConfig::for_test(data.clone())).await?;
    let client = reqwest::Client::new();
    let base = format!("http://{}", hub.addr);
    let human = hub.mint_device_token("workspace-phone").await?;
    let bot = hub.mint_bot_device_token("workspace-bot").await?;
    let enroll = hub.mint_enroll_token(5).await?;
    let host = HostId::new().as_id().as_str().to_owned();
    let url = format!("{base}/v1/hosts/{host}/workspaces");
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    node.send(Message::Text(json!({
        "jsonrpc":"2.0", "id":"hello", "method":"node.hello", "params":{
            "hostId":host, "host":{"hostname":"workspace-node", "workspaces":[], "workspaceRevision":0,
                "herdr":{"path":"/usr/bin/herdr"},
                "cli":[{"kind":"claude", "path":"/usr/bin/claude", "auth":"logged_in"}]}
        }
    }).to_string().into())).await?;
    let hello = node.next().await.context("hello")??;
    assert!(hello.into_text()?.contains("result"));
    let (created_tx, mut created_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_for_node = host.clone();
    let task = tokio::spawn(async move {
        let mut revision = 0;
        let mut workspaces = json!([]);
        while let Some(Ok(Message::Text(text))) = node.next().await {
            let frame: Value = serde_json::from_str(&text).unwrap();
            let method = frame["method"].as_str().unwrap_or("");
            let params = &frame["params"];
            let response = match method {
                "workspace.list" => {
                    json!({"result":{"workspaceRevision":revision,"workspaces":workspaces}})
                }
                "workspace.register" | "workspace.unregister" => {
                    assert!(params["commandId"].as_str().unwrap().starts_with("cmd_"));
                    if params["path"] == "/outside" {
                        json!({"error":{"code":-32602,"message":"workspace /outside is outside workspace_roots; allowed roots: /tmp"}})
                    } else if params["path"] == "/unsettled" || params["phase"] == "prepare" {
                        json!({"result":{"commandId":params["commandId"],"phase":"prepared","workspaceRevision":revision,"workspaces":workspaces}})
                    } else {
                        assert_eq!(params["phase"], "commit");
                        revision += 1;
                        workspaces = if method == "workspace.register" {
                            json!([{"workspaceId":"wsp_project","hostId":host_for_node,"root":params["path"]}])
                        } else {
                            json!([])
                        };
                        json!({"result":{"commandId":params["commandId"],"phase":"settled","workspaceRevision":revision,"workspaces":workspaces}})
                    }
                }
                "instance.create" => {
                    created_tx.send(params.clone()).unwrap();
                    json!({"result":{"accepted":true}})
                }
                _ => continue,
            };
            let mut response = response;
            response["jsonrpc"] = json!("2.0");
            response["id"] = frame["id"].clone();
            node.send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
            if params["phase"] == "commit" && revision > 0 {
                node.send(Message::Text(json!({"jsonrpc":"2.0", "id":"stale-heartbeat", "method":"node.heartbeat", "params":{"host":{"workspaceRevision":revision-1,"workspaces":[]}}}).to_string().into())).await.unwrap();
            }
        }
    });

    let listed: Value = client
        .get(&url)
        .bearer_auth(&bot)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(listed["workspaces"], json!([]));
    let bad = client
        .post(&url)
        .bearer_auth(&human)
        .json(&json!({"path":"/outside"}))
        .send()
        .await?;
    assert_eq!(bad.status(), 400);
    assert!(bad.text().await?.contains("allowed roots: /tmp"));
    let unacknowledged = client
        .post(&url)
        .bearer_auth(&human)
        .json(&json!({"path":"/unsettled"}))
        .send()
        .await?;
    assert_eq!(unacknowledged.status(), 400);
    assert!(unacknowledged.text().await?.contains("as settled"));
    let db = rusqlite::Connection::open(data.join("hub.sqlite"))?;
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM workspace_journal WHERE revision IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        count, 1,
        "only initial inventory was journaled before mutation settlement"
    );

    let mut follow_request = format!("ws://{}/v1/follow", hub.addr).into_client_request()?;
    follow_request
        .headers_mut()
        .insert("Authorization", format!("Bearer {human}").parse()?);
    let (mut follow, _) = tokio_tungstenite::connect_async(follow_request).await?;
    let registered: Value = client
        .post(&url)
        .bearer_auth(&human)
        .json(&json!({"path":"/tmp/project"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(registered["workspaces"][0]["root"], "/tmp/project");
    let update = tokio::time::timeout(Duration::from_secs(5), follow.next())
        .await?
        .context("follow workspace event")??;
    let update: Value = serde_json::from_str(&update.into_text()?)?;
    assert_eq!(update["event"]["type"], "host.updated");
    assert_eq!(update["event"]["workspaces"], registered["workspaces"]);
    follow.close(None).await?;

    let projected: Value = client
        .get(format!("{base}/v1/hosts/{host}"))
        .bearer_auth(&human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(projected["workspaces"], registered["workspaces"]);
    let created: Value = client.post(format!("{base}/v1/instances")).bearer_auth(&human).json(&json!({
        "hostId":host,"kind":"claude","driver":"claude-headless","workspaceId":"wsp_project","cwd":"src","prompt":"hello"
    })).send().await?.error_for_status()?.json().await?;
    let launch = tokio::time::timeout(Duration::from_secs(5), created_rx.recv())
        .await?
        .context("instance launch")?;
    assert_eq!(launch["spec"]["workspaceId"], "wsp_project");
    assert_eq!(launch["spec"]["cwd"], "src");
    assert_eq!(created["instance"]["workspaceId"], "wsp_project");
    let agent = launch["agentCredential"]["token"]
        .as_str()
        .context("agent credential")?;
    for method in [
        reqwest::Method::GET,
        reqwest::Method::POST,
        reqwest::Method::DELETE,
    ] {
        let response = client
            .request(method, &url)
            .bearer_auth(agent)
            .json(&json!({"path":"/tmp/project"}))
            .send()
            .await?;
        assert_eq!(response.status(), 403);
    }
    let hostile_origin = client
        .post(&url)
        .bearer_auth(&human)
        .header("origin", "https://untrusted.invalid")
        .json(&json!({"path":"/tmp/project"}))
        .send()
        .await?;
    assert_eq!(hostile_origin.status(), 403);
    let removed: Value = client
        .delete(&url)
        .bearer_auth(&bot)
        .json(&json!({"path":"/tmp/project"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(removed["workspaces"], json!([]));
    let again: Value = client
        .post(&url)
        .bearer_auth(&human)
        .json(&json!({"path":"/tmp/project"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(again["workspaces"], registered["workspaces"]);
    task.abort();
    let _ = task.await;
    drop(db);
    hub.shutdown().await;

    let hub = spawn(HubConfig::for_test(data.clone())).await?;
    let url = format!("http://{}/v1/hosts/{host}/workspaces", hub.addr);
    let persisted: Value = client
        .get(&url)
        .bearer_auth(&human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(persisted["workspaces"], registered["workspaces"]);
    assert_eq!(
        client
            .delete(&url)
            .bearer_auth(&bot)
            .json(&json!({"path":"/tmp/project"}))
            .send()
            .await?
            .status(),
        409
    );
    let db = rusqlite::Connection::open(data.join("hub.sqlite"))?;
    let settled: i64 = db.query_row("SELECT COUNT(*) FROM commands WHERE operation = 'workspace.register' AND state = 'settled'", [], |row| row.get(0))?;
    assert_eq!(settled, 2);
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM workspace_journal WHERE command_id IS NOT NULL AND revision IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 3);
    let rejected: (String, String) = db.query_row("SELECT state, resolution FROM commands WHERE operation = 'workspace.register' AND payload_json LIKE '%/outside%'", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    assert_eq!(rejected, ("queued".into(), "clear".into()));
    let rejection: String = db.query_row(
        "SELECT payload_json FROM workspace_journal WHERE revision IS NULL",
        [],
        |row| row.get(0),
    )?;
    assert!(rejection.contains("allowed roots: /tmp"));
    drop(db);
    hub.shutdown().await;
    Ok(())
}
