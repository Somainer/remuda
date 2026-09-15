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

/// claude-print also emits hook lifecycles whose `nativeId` is a hook id, not
/// a session. Treating one as a session records an id `--resume` rejects.
fn hook_started(hook_id: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "payload": {
            "type": "native",
            "topic": "hook",
            "nativeName": "hook_started",
            "nativeId": { "state": "known", "value": hook_id },
            "status": { "state": "known", "value": "started" },
            "relatedIds": { "hookEvent": "SessionStart", "hookId": hook_id },
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

struct FakeNode {
    launches: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    async fn connect(hub: &remuda_hub::RunningHub, host: &str) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
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

        let (launch_tx, launch_rx) = tokio::sync::mpsc::unbounded_channel();
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

        Ok(Self {
            launches: launch_rx,
            appends: append_tx,
            task: node_task,
        })
    }

    async fn launch(&mut self) -> Result<(String, Value)> {
        tokio::time::timeout(Duration::from_secs(5), self.launches.recv())
            .await?
            .context("launch forwarded")
    }
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn assert_no_provider_secret(value: &Value, secrets: &[&str]) {
    let body = value.to_string();
    assert!(
        !body.contains("providerAuthToken"),
        "provider credential field must stay on the Node RPC"
    );
    for secret in secrets {
        assert!(
            !body.contains(secret),
            "provider credential must not appear in public or persisted data"
        );
    }
}

async fn finish_session(
    node: &FakeNode,
    client: &reqwest::Client,
    base: &str,
    human: &str,
    instance_id: &str,
    session_id: &str,
) -> Result<()> {
    node.appends.send((
        instance_id.to_owned(),
        session_started(session_id, "/tmp/resume-transcript.jsonl"),
    ))?;
    node.appends.send((instance_id.to_owned(), exited()))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view: Value = client
                .get(format!("{base}/v1/instances/{instance_id}"))
                .bearer_auth(human)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if view["nativeSessionId"] == session_id && view["lifecycle"] == "exited" {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn resume_creates_a_linked_child_on_both_targets_and_refuses_agents() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let client = reqwest::Client::new();
    let base = format!("http://{}", hub.addr);
    let human = hub.mint_device_token("resume-phone").await?;
    let host = HostId::new().as_id().as_str().to_owned();
    let mut node = FakeNode::connect(&hub, &host).await?;

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
    let (create_method, create_params) = node.launch().await?;
    assert_eq!(create_method, "instance.create");
    assert!(create_params["spec"].get("providerOverlay").is_none());
    assert!(create_params["spec"].get("providerAuthToken").is_none());
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
    node.appends
        .send((parent.clone(), session_started(session, "/tmp/t.jsonl")))?;
    // Hook lifecycles arrive after the session one and must not overwrite it.
    node.appends.send((
        parent.clone(),
        hook_started("f121381b-c2c9-47fd-a253-f5efea4e4c94"),
    ))?;
    node.appends.send((parent.clone(), exited()))?;
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

    let (method, params) = node.launch().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["resumeSessionId"], json!(session));
    assert_eq!(params["spec"]["resumedFrom"], json!(parent));
    assert_eq!(params["spec"]["driver"], json!("claude-print"));
    assert!(params["spec"].get("providerOverlay").is_none());
    assert!(params["spec"].get("providerAuthToken").is_none());

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
    let (method, params) = node.launch().await?;
    assert_eq!(method, "instance.resume");
    assert_eq!(params["spec"]["resumeSessionId"], json!(session));
    assert_eq!(params["spec"]["driver"], json!("claude-pty"));
    assert!(params["spec"].get("providerOverlay").is_none());
    assert!(params["spec"].get("providerAuthToken").is_none());

    let bad_mode = client
        .post(&resume_url)
        .bearer_auth(&human)
        .json(&json!({"mode":"telepathy"}))
        .send()
        .await?;
    assert_eq!(bad_mode.status(), 400);

    let missing = client
        .post(format!(
            "{base}/v1/instances/ins_00000000-0000-7000-8000-000000000000/resume"
        ))
        .bearer_auth(&human)
        .json(&json!({"mode":"structured"}))
        .send()
        .await?;
    assert_eq!(missing.status(), 404);

    Ok(())
}

#[tokio::test]
async fn resume_redelivers_current_gateway_profile_and_host_scoped_secret() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("hub");
    let hub = spawn(HubConfig::for_test(data.clone())).await?;
    let client = reqwest::Client::new();
    let base = format!("http://{}", hub.addr);
    let human = hub.mint_device_token("resume-provider-phone").await?;
    let host = HostId::new().as_id().as_str().to_owned();
    let scope = format!("host:{host}");
    let mut node = FakeNode::connect(&hub, &host).await?;
    let secrets = [
        "fake-resume-provider-initial",
        "fake-resume-provider-structured",
        "fake-resume-provider-terminal",
    ];
    let profile: Value = client
        .post(format!("{base}/v1/providers"))
        .bearer_auth(&human)
        .json(&json!({
            "name": "resume-gateway",
            "kind": "gateway",
            "baseUrl": "https://initial.example.invalid/v1",
            "models": ["initial-model"],
            "defaultModel": "initial-model",
            "authToken": secrets[0],
            "scope": scope
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_no_provider_secret(&profile, &secrets);
    let profile_id = profile["id"].as_str().context("provider id")?;
    let provider_url = format!("{base}/v1/providers/{profile_id}");
    let created: Value = client
        .post(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .json(&json!({
            "hostId": host,
            "kind": "claude",
            "driver": "claude-print",
            "delegation": "gateway",
            "providerProfileId": profile_id,
            "prompt": "hi"
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_no_provider_secret(&created, &secrets);
    let parent = created["instance"]["instanceId"]
        .as_str()
        .context("parent id")?;
    let (method, params) = node.launch().await?;
    assert_eq!(method, "instance.create");
    assert_eq!(params["spec"]["delegation"], "gateway");
    assert_eq!(params["spec"]["providerAuthToken"], secrets[0]);
    assert_eq!(params["spec"]["providerOverlay"]["profileId"], profile_id);
    assert_eq!(params["spec"]["providerOverlay"]["scope"], scope);
    assert_eq!(
        params["spec"]["providerOverlay"]["baseUrl"],
        "https://initial.example.invalid/v1"
    );
    let session = "01993ab0-0000-7000-8000-0000000000dd";
    finish_session(&node, &client, &base, &human, parent, session).await?;

    let mut terminal_child = String::new();
    for (index, (mode, driver)) in [("structured", "claude-print"), ("terminal", "claude-pty")]
        .into_iter()
        .enumerate()
    {
        // Resume must resolve current profile metadata and release the current
        // secret, rather than reuse the parent's persisted public snapshot.
        let base_url = format!("https://{mode}.example.invalid/v1");
        let model = format!("{mode}-model");
        let patched: Value = client
            .patch(&provider_url)
            .bearer_auth(&human)
            .json(&json!({
                "baseUrl": base_url,
                "models": [model],
                "defaultModel": model,
                "headers": {"X-Resume-Revision": mode},
                "authToken": secrets[index + 1]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_no_provider_secret(&patched, &secrets);
        let resumed: Value = client
            .post(format!("{base}/v1/instances/{parent}/resume"))
            .bearer_auth(&human)
            .json(&json!({"mode": mode}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_no_provider_secret(&resumed, &secrets);
        assert_eq!(resumed["replayed"], false);
        let child = resumed["instance"]["instanceId"]
            .as_str()
            .context("resumed child id")?;
        assert_ne!(child, parent);
        let (method, params) = node.launch().await?;
        assert_eq!(method, "instance.resume");
        assert_eq!(params["spec"]["resumeSessionId"], session);
        assert_eq!(params["spec"]["resumedFrom"], parent);
        assert_eq!(params["spec"]["driver"], driver);
        assert_eq!(params["spec"]["delegation"], "gateway");
        assert_eq!(params["spec"]["providerProfileId"], profile_id);
        assert_eq!(params["spec"]["providerAuthToken"], secrets[index + 1]);
        assert_eq!(
            params["spec"]["providerOverlay"],
            json!({
                "profileId": profile_id,
                "kind": "gateway",
                "baseUrl": base_url,
                "model": model,
                "headers": {"X-Resume-Revision": mode},
                "scope": scope
            })
        );
        assert!(params["agentCredential"]["token"].as_str().is_some());
        if mode == "terminal" {
            terminal_child = child.to_owned();
        }
    }

    // Moving the profile to another host must reject a new resume before any
    // instance/command is allocated or credentials reach the original Node.
    finish_session(&node, &client, &base, &human, &terminal_child, session).await?;
    let other_host = HostId::new().as_id().as_str().to_owned();
    let mut other_node = FakeNode::connect(&hub, &other_host).await?;
    let moved: Value = client
        .patch(&provider_url)
        .bearer_auth(&human)
        .json(&json!({"scope": format!("host:{other_host}")}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_no_provider_secret(&moved, &secrets);
    let rejected = client
        .post(format!("{base}/v1/instances/{terminal_child}/resume"))
        .bearer_auth(&human)
        .json(&json!({"mode": "terminal"}))
        .send()
        .await?;
    assert_eq!(rejected.status(), 422);
    let rejected: Value = rejected.json().await?;
    assert_no_provider_secret(&rejected, &secrets);
    assert!(rejected.to_string().contains("host-scoped profile"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), node.launches.recv())
            .await
            .is_err(),
        "scope rejection must not send a launch to the original Node"
    );
    assert!(
        other_node.launches.try_recv().is_err(),
        "resume must not relocate to the profile's new host"
    );

    let public_instances: Value = client
        .get(format!("{base}/v1/instances"))
        .bearer_auth(&human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_no_provider_secret(&public_instances, &secrets);
    assert_eq!(
        public_instances["items"]
            .as_array()
            .context("instances")?
            .len(),
        3
    );
    let conn = rusqlite::Connection::open(data.join("hub.sqlite"))?;
    let mut instances = conn.prepare("SELECT spec_json FROM instances")?;
    let specs: Vec<String> = instances
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(specs.len(), 3);
    let mut commands = conn.prepare("SELECT payload_json FROM commands")?;
    let payloads: Vec<String> = commands
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(payloads.len(), 3);
    for raw in specs.iter().chain(&payloads) {
        assert_no_provider_secret(&serde_json::from_str(raw)?, &secrets);
    }
    Ok(())
}
