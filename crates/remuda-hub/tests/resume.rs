//! D-026 resume route: native session projection, both targets, scope and refusals.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Journal event a Node emits when a driver reports the session it is running.
fn session_started(session_id: &str, transcript: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "session",
            "nativeName": "session",
            "nativeId": { "state": "known", "value": session_id },
            "status": { "state": "known", "value": "started" },
            "relatedIds": { "transcriptPath": transcript },
            "dataRef": null,
            "severity": "info",
            "affectsCompletion": false
        }
    })
}

fn exited() -> Value {
    json!({
        "kind": "lifecycle",
        "payload": { "type": "entity", "state": "exited", "reasonCode": "native-exit" }
    })
}

#[tokio::test]
async fn resume_creates_a_linked_child_on_both_targets_and_refuses_agents() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let client = reqwest::Client::new();
    let base = format!("http://{}", hub.addr);
    let human = hub.mint_device_token("resume-phone").await?;
    let enroll = hub.mint_enroll_token(5).await?;
    let host = HostId::new().as_id().as_str().to_owned();

    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc":"2.0", "id":"hello", "method":"node.hello",
            "params": { "hostId": host, "host": {
                "hostname":"resume-node", "workspaces":[], "workspaceRevision":0,
                "herdr":{"path":"/usr/bin/herdr"},
                "cli":[{"kind":"claude","path":"/usr/bin/claude","auth":"logged_in"}]
            }}
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = node.next().await.context("hello")??;
    assert!(hello.into_text()?.contains("result"));

    let (launch_tx, mut launch_rx) = tokio::sync::mpsc::unbounded_channel();
    let (append_tx, mut append_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let node_task = tokio::spawn(async move {
        // Journal appends the fake Node must issue, fed from the test body.
        loop {
            tokio::select! {
                Some((instance_id, event)) = append_rx.recv() => {
                    node.send(Message::Text(json!({
                        "jsonrpc":"2.0","id":"append","method":"journal.append",
                        "params":{"instanceId":instance_id,"event":event}
                    }).to_string().into())).await.unwrap();
                }
                frame = node.next() => {
                    let Some(Ok(Message::Text(text))) = frame else { break };
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    let method = frame["method"].as_str().unwrap_or("");
                    if method != "instance.create" && method != "instance.resume" {
                        continue;
                    }
                    launch_tx.send((method.to_owned(), frame["params"].clone())).unwrap();
                    node.send(Message::Text(json!({
                        "jsonrpc":"2.0","id":frame["id"],"result":{"accepted":true}
                    }).to_string().into())).await.unwrap();
                }
            }
        }
    });

    let created: Value = client
        .post(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .json(&json!({"hostId":host,"kind":"claude","driver":"claude-print","prompt":"hi"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let parent = created["instance"]["instanceId"]
        .as_str()
        .context("parent id")?
        .to_owned();
    let (create_method, create_params) = tokio::time::timeout(Duration::from_secs(5), launch_rx.recv())
        .await?
        .context("create forwarded")?;
    assert_eq!(create_method, "instance.create");
    let agent = create_params["agentCredential"]["token"]
        .as_str()
        .context("agent credential")?
        .to_owned();
    let resume_url = format!("{base}/v1/instances/{parent}/resume");

    // Before any driver reported a session, there is nothing to resume and the
    // caller must be told that rather than getting an empty conversation.
    let premature = client
        .post(&resume_url)
        .bearer_auth(&human)
        .json(&json!({"mode":"structured"}))
        .send()
        .await?;
    assert_eq!(premature.status(), 409);
    assert!(premature.text().await?.contains("native session id"));

    let session = "01993ab0-0000-7000-8000-0000000000cc";
    append_tx.send((parent.clone(), session_started(session, "/tmp/t.jsonl")))?;
    append_tx.send((parent.clone(), exited()))?;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view: Value = client
                .get(format!("{base}/v1/instances/{parent}"))
                .bearer_auth(&human)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if view["nativeSessionId"] == json!(session) && view["lifecycle"] == json!("exited") {
                return anyhow::Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    assert_eq!(view["nativeTranscriptPath"], json!("/tmp/t.jsonl"));

    // Agents never resume: it spends host capacity and starts a native process
    // outside the scope their instance credential was issued for (D-017).
    let forbidden = client
        .post(&resume_url)
        .bearer_auth(&agent)
        .header("x-remuda-instance-id", &parent)
        .json(&json!({"mode":"structured"}))
        .send()
        .await?;
    assert_eq!(forbidden.status(), 403);

    let structured: Value = client
        .post(&resume_url)
        .bearer_auth(&human)
        .json(&json!({"mode":"structured"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(structured["mode"], json!("structured"));
    assert_eq!(structured["replayed"], json!(false));
    let child = structured["instance"]["instanceId"]
        .as_str()
        .context("child id")?
        .to_owned();
    assert_ne!(child, parent, "resume must not reuse the exited instance");
    assert_eq!(structured["instance"]["driver"], json!("claude-print"));
    assert_eq!(structured["instance"]["hostId"], json!(host));
    assert_eq!(structured["instance"]["resumedFrom"], json!(parent));

    let (method, params) = tokio::time::timeout(Duration::from_secs(5), launch_rx.recv())
        .await?
        .context("resume forwarded")?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["resumeSessionId"], json!(session));
    assert_eq!(params["spec"]["resumedFrom"], json!(parent));
    assert_eq!(params["spec"]["driver"], json!("claude-print"));

    // A repeat inside the idempotency window reuses the child instead of
    // launching a second process against the same conversation.
    let replay: Value = client
        .post(&resume_url)
        .bearer_auth(&human)
        .json(&json!({"mode":"structured"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(replay["instance"]["instanceId"], json!(child));

    // The terminal target answers "can this agent session go back to a
    // terminal?": same native session, claude-pty driver.
    let terminal: Value = client
        .post(&resume_url)
        .bearer_auth(&human)
        .json(&json!({"mode":"terminal"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(terminal["mode"], json!("terminal"));
    assert_eq!(terminal["instance"]["driver"], json!("claude-pty"));
    assert_ne!(terminal["instance"]["instanceId"], json!(child));
    let (method, params) = tokio::time::timeout(Duration::from_secs(5), launch_rx.recv())
        .await?
        .context("terminal resume forwarded")?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["resumeSessionId"], json!(session));
    assert_eq!(params["spec"]["driver"], json!("claude-pty"));

    let bad_mode = client
        .post(&resume_url)
        .bearer_auth(&human)
        .json(&json!({"mode":"telepathy"}))
        .send()
        .await?;
    assert_eq!(bad_mode.status(), 400);

    let missing = client
        .post(format!("{base}/v1/instances/ins_00000000-0000-7000-8000-000000000000/resume"))
        .bearer_auth(&human)
        .json(&json!({"mode":"structured"}))
        .send()
        .await?;
    assert_eq!(missing.status(), 404);

    node_task.abort();
    Ok(())
}
