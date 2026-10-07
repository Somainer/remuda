//! `ma-seat-cli` (D-057 §3.2): the seating create against the real Hub.
//!
//! One case, the Hub side of the CLI flags: the full seating body round-trips
//! through `GET /v1/instances/{id}` (grants, scope, `projectId`, permission
//! mode, model, restart), `restart` from an Agent credential is refused with
//! a 403 that carries the Hub's reason, and a second live `address-owner`
//! holder gets the 409 conflict text.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, ProjectId, WorkspaceId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// A fake Node that accepts every Hub RPC and records its frames.
struct FakeNode {
    frames: mpsc::UnboundedReceiver<(String, Value)>,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    async fn connect(hub: &remuda_hub::RunningHub, host: &str) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
        ws.send(Message::Text(
            json!({
                "jsonrpc":"2.0", "id":"hello", "method":"node.hello",
                "params": { "hostId": host, "host": {
                    "hostname":"seat-node", "workspaces":[], "workspaceRevision":0,
                    "herdr":{"path":"/usr/bin/herdr"},
                    "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged_in"}]
                }}
            })
            .to_string()
            .into(),
        ))
        .await?;
        let hello: Value = serde_json::from_str(&ws.next().await.context("hello")??.into_text()?)
            .context("hello json")?;
        assert!(hello.get("result").is_some(), "hello failed: {hello}");

        let (frame_tx, frame_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                let Ok(Message::Text(text)) = message else {
                    continue;
                };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(method) = frame["method"].as_str() else {
                    continue;
                };
                if frame_tx
                    .send((method.to_owned(), frame["params"].clone()))
                    .is_err()
                {
                    break;
                }
                // Accept every RPC: launches, closes, sends.
                if ws
                    .send(Message::Text(
                        json!({"jsonrpc":"2.0","id":frame["id"],"result":{"accepted":true}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            frames: frame_rx,
            _task: task,
        })
    }

    async fn next_frame(&mut self) -> Result<(String, Value)> {
        tokio::time::timeout(Duration::from_secs(5), self.frames.recv())
            .await?
            .context("frame")
    }
}

#[tokio::test]
async fn seating_create_round_trips_restart_is_human_only_and_owner_is_unique() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
    let human = hub.mint_device_token("seat-phone").await?;
    let host = HostId::new();
    let project = ProjectId::new();
    let workspace = WorkspaceId::new();
    let mut node = FakeNode::connect(&hub, host.as_id().as_str()).await?;
    let http = reqwest::Client::new();
    let base = format!("http://{}", hub.addr);

    // 1. The owner seats Main with every ma-seat-cli flag represented.
    let body = json!({
        "hostId": host.as_id().as_str(),
        "kind": "claude",
        "driver": "claude-sdk",
        "name": "main",
        "title": "Main",
        "grants": ["address-owner", "dispatch", "land", "spend"],
        "scope": {
            "projectIds": [project.as_id().as_str()],
            "hostIds": [host.as_id().as_str()],
            "workspaceIds": [workspace.as_id().as_str()],
        },
        "permissionMode": "acceptEdits",
        "model": "claude-seat-model",
        "restart": { "onProcessLoss": true, "maxPerHour": 3 },
        "prompt": "seat brief",
    });
    let created = http
        .post(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .json(&body)
        .send()
        .await?;
    assert!(created.status().is_success(), "{}", created.text().await?);
    let created: Value = created.json().await?;
    let seat_id = created["instance"]["instanceId"]
        .as_str()
        .context("seat id")?
        .to_owned();
    let (method, params) = node.next_frame().await?;
    assert_eq!(method, "instance.create");
    let agent_token = params["agentCredential"]["token"]
        .as_str()
        .context("launch credential")?
        .to_owned();

    // 2. Every seating field round-trips into the GET projection.
    let view: Value = http
        .get(format!("{base}/v1/instances/{seat_id}"))
        .bearer_auth(&human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        view["grants"],
        json!(["address-owner", "dispatch", "land", "spend"])
    );
    assert_eq!(
        view["scope"],
        json!({
            "projectIds": [project.as_id().as_str()],
            "hostIds": [host.as_id().as_str()],
            "workspaceIds": [workspace.as_id().as_str()],
        })
    );
    // The single-project convenience projection.
    assert_eq!(view["projectId"], json!(project.as_id().as_str()));
    assert_eq!(view["permissionMode"], json!("acceptEdits"));
    assert_eq!(view["model"], json!("claude-seat-model"));
    assert_eq!(
        view["restart"],
        json!({ "onProcessLoss": true, "maxPerHour": 3 })
    );

    // 3. The seat's own Agent credential cannot set `restart`: 403 with the
    //    Hub's reason (D-057 §6.1), not a bare status.
    let denied = http
        .post(format!("{base}/v1/instances"))
        .bearer_auth(&agent_token)
        .json(&json!({
            "hostId": host.as_id().as_str(),
            "kind": "claude",
            "driver": "claude-print",
            "permissionMode": "manual",
            "restart": { "onProcessLoss": true, "maxPerHour": 3 },
            "prompt": "agents may not seat continuations",
        }))
        .send()
        .await?;
    assert_eq!(denied.status(), 403);
    let denied_body: Value = denied.json().await?;
    let reason = denied_body["error"]
        .as_str()
        .context("403 body needs an error reason for the CLI to print")?;
    assert!(
        reason.contains("Human creator"),
        "reason explains the boundary: {reason}"
    );

    // 4. A second live address-owner holder is refused with the conflict text.
    let conflict = http
        .post(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .json(&json!({
            "hostId": host.as_id().as_str(),
            "kind": "claude",
            "driver": "claude-print",
            "grants": ["address-owner"],
            "prompt": "a rival seat",
        }))
        .send()
        .await?;
    assert_eq!(conflict.status(), 409);
    let conflict_body: Value = conflict.json().await?;
    assert_eq!(conflict_body["code"], json!("COMMAND_ID_CONFLICT"));
    assert!(
        conflict_body["error"]
            .as_str()
            .unwrap_or("")
            .contains("address-owner"),
        "conflict names the grant: {conflict_body}"
    );

    Ok(())
}
