//! ma-skill (D-060 item 5): the launch facts the Hub stamps as `skillContext`.
//!
//! On every forwarded `instance.create` / `instance.resume` of an agent kind,
//! to a Node that advertises `capabilities.skillContext`, the Hub stamps the
//! instance's grants, whether its host advertises herdr, the project's D-051
//! switch and whether an inbox exists, all read from its own rows. A copy in
//! a request body never reaches the Node; an older Node gets no stamp. Every
//! case drives the Hub over HTTP with a scripted fake Node that records the
//! frames it receives.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8000-0000000000cc";

/// The D-051 switch is process-global; the cases that read it take turns.
static SWITCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct FakeNode {
    frames: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    appends: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeNode {
    /// Enroll and say hello. `herdr` puts a herdr record in the host
    /// inventory; `capabilities` is the hello's blob, if any.
    async fn connect(
        hub: &remuda_hub::RunningHub,
        host: &str,
        herdr: bool,
        capabilities: Option<Value>,
    ) -> Result<Self> {
        let enroll = hub.mint_enroll_token(5).await?;
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut inventory = json!({
            "hostname": "skill-node", "workspaces": [], "workspaceRevision": 0,
            "cli": [{"kind": "claude", "path": "/usr/bin/claude", "auth": "logged_in"}]
        });
        if herdr {
            inventory["herdr"] = json!({"path": "/usr/bin/herdr"});
        }
        let mut params = json!({ "hostId": host, "host": inventory });
        if let Some(capabilities) = capabilities {
            params["capabilities"] = capabilities;
        }
        node.send(Message::Text(
            json!({"jsonrpc": "2.0", "id": "hello", "method": "node.hello", "params": params})
                .to_string()
                .into(),
        ))
        .await?;
        let hello: Value =
            serde_json::from_str(&node.next().await.context("hello")??.into_text()?)?;
        anyhow::ensure!(hello.get("result").is_some(), "hello failed: {hello}");
        let (frame_tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let (appends, mut append_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((instance_id, event)) = append_rx.recv() => {
                        let frame = json!({
                            "jsonrpc": "2.0", "id": "append", "method": "journal.append",
                            "params": {"instanceId": instance_id, "event": event}
                        });
                        if node.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    frame = node.next() => {
                        let Some(Ok(Message::Text(text))) = frame else {
                            if matches!(frame, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) {
                                break;
                            }
                            continue;
                        };
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                        let Some(method) = frame["method"].as_str() else { continue };
                        if frame_tx.send((method.to_owned(), frame["params"].clone())).is_err() {
                            break;
                        }
                        let reply = json!({"jsonrpc": "2.0", "id": frame["id"], "result": {"accepted": true}});
                        if node.send(Message::Text(reply.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            frames,
            appends,
            task,
        })
    }

    /// The next frame named `method`, skipping any other.
    async fn next(&mut self, method: &str) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (name, params) = self.frames.recv().await.context("node closed")?;
                if name == method {
                    return anyhow::Ok(params);
                }
            }
        })
        .await?
    }
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let human = hub.mint_device_token("skill-owner").await?;
        Ok(Self {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host: HostId::new().as_id().as_str().to_owned(),
        })
    }

    async fn ok(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("http://{}{path}", self.hub.addr))
            .bearer_auth(&self.human)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        anyhow::ensure!(status == 200, "POST {path} → {status}: {text}");
        Ok(serde_json::from_str(&text)?)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("http://{}{path}", self.hub.addr))
            .bearer_auth(&self.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Create `kind` on `driver` with `extra` merged into the body; returns
    /// its id and the forwarded `instance.create` params.
    async fn create(
        &self,
        node: &mut FakeNode,
        kind: &str,
        driver: &str,
        extra: Value,
    ) -> Result<(String, Value)> {
        let mut body = json!({
            "hostId": self.host,
            "kind": kind,
            "driver": driver,
            "prompt": "seat brief",
        });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                body.insert(key.clone(), value.clone());
            }
        }
        let created = self.ok("/v1/instances", body).await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("instance id")?
            .to_owned();
        let params = node.next("instance.create").await?;
        assert_eq!(params["instanceId"], json!(id));
        Ok((id, params))
    }

    /// Report the native session and its exit, so the owner can resume.
    async fn end(&self, node: &FakeNode, instance: &str) -> Result<()> {
        node.appends.send((
            instance.to_owned(),
            json!({"kind": "lifecycle", "payload": {
                "type": "native", "topic": "session", "nativeName": "session",
                "nativeId": {"state": "known", "value": SESSION},
                "status": {"state": "known", "value": "started"},
                "relatedIds": {}, "dataRef": null, "severity": "info",
                "affectsCompletion": false
            }}),
        ))?;
        node.appends.send((
            instance.to_owned(),
            json!({"kind": "lifecycle", "payload": {
                "type": "entity", "state": "exited", "reasonCode": "native-exit"
            }}),
        ))?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let view = self.get(&format!("/v1/instances/{instance}")).await?;
                if view["nativeSessionId"] == json!(SESSION) && view["lifecycle"] == json!("exited")
                {
                    return anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?
    }
}

/// A current Node: it carries both Hub-stamped launch fields.
fn capable() -> Option<Value> {
    Some(json!({"memoryContext": true, "skillContext": true, "lineageFence": true}))
}

/// The stamp is the instance's own grants and its host's herdr record, at the
/// top level of the params and never inside the spec; a resume of the same
/// seat carries it again.
#[tokio::test]
async fn a_capable_node_gets_the_seat_facts_on_create_and_resume() -> Result<()> {
    let _switch = SWITCH.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true, capable()).await?;
    let (seat, params) = ctx
        .create(
            &mut node,
            "claude",
            "claude-sdk",
            json!({"grants": ["dispatch", "land"]}),
        )
        .await?;
    assert_eq!(
        params["skillContext"],
        json!({"grants": ["dispatch", "land"], "herdr": true,
               "delegatedDecisions": false, "inbox": false}),
        "{params}"
    );
    assert!(params["spec"].get("skillContext").is_none(), "{params}");

    ctx.end(&node, &seat).await?;
    ctx.ok(&format!("/v1/instances/{seat}/resume"), json!({}))
        .await?;
    let resumed = node.next("instance.resume").await?;
    assert_eq!(
        resumed["skillContext"]["grants"],
        json!(["dispatch", "land"]),
        "{resumed}"
    );
    assert_eq!(resumed["skillContext"]["herdr"], json!(true), "{resumed}");
    Ok(())
}

/// Without grants and on a host with no herdr record, the facts say so; the
/// D-051 switch is read as the answer gate reads it for the project.
#[tokio::test]
async fn the_facts_follow_grants_herdr_and_the_d051_switch() -> Result<()> {
    let _switch = SWITCH.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (_, params) = ctx
        .create(&mut node, "codex", "generic-pty", json!({}))
        .await?;
    assert_eq!(
        params["skillContext"],
        json!({"grants": [], "herdr": false, "delegatedDecisions": false, "inbox": false}),
        "{params}"
    );

    remuda_hub::delegated_decisions_test_support::set_global_override(Some(true));
    let on = ctx
        .create(
            &mut node,
            "claude",
            "claude-sdk",
            json!({"grants": ["dispatch"]}),
        )
        .await;
    remuda_hub::delegated_decisions_test_support::set_global_override(None);
    let (_, params) = on?;
    assert_eq!(
        params["skillContext"]["delegatedDecisions"],
        json!(true),
        "{params}"
    );
    assert_eq!(params["skillContext"]["grants"], json!(["dispatch"]));
    Ok(())
}

/// `skillContext` is Hub-generated: a copy at the top level of a create body
/// or inside its spec never reaches the Node, and neither does one in a
/// command payload.
#[tokio::test]
async fn a_body_supplied_skill_context_is_stripped() -> Result<()> {
    let _switch = SWITCH.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let forged = json!({"grants": ["land", "dispatch", "FORGED"], "herdr": true,
                        "delegatedDecisions": true, "inbox": true});
    let (seat, params) = ctx
        .create(
            &mut node,
            "claude",
            "claude-sdk",
            json!({"skillContext": forged, "spec": {"skillContext": forged}}),
        )
        .await?;
    assert_eq!(
        params["skillContext"],
        json!({"grants": [], "herdr": false, "delegatedDecisions": false, "inbox": false}),
        "{params}"
    );
    assert!(!params.to_string().contains("FORGED"), "{params}");

    ctx.ok(
        &format!("/v1/instances/{seat}/commands"),
        json!({"operation": "instance.send",
               "payload": {"input": {"type": "prompt", "text": "hello"},
                           "skillContext": forged}}),
    )
    .await?;
    let send = node.next("instance.send").await?;
    assert!(send.get("skillContext").is_none(), "{send}");
    assert!(!send.to_string().contains("FORGED"), "{send}");
    Ok(())
}

/// A Node whose hello does not advertise `capabilities.skillContext` would
/// drop the field silently, so it gets none; nor does a hello with no
/// capabilities at all, even after a newer build on the same host claimed it.
#[tokio::test]
async fn an_old_node_without_the_capability_gets_no_stamp() -> Result<()> {
    let _switch = SWITCH.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(
        &ctx.hub,
        &ctx.host,
        true,
        Some(json!({"memoryContext": true, "lineageFence": true})),
    )
    .await?;
    let (_, params) = ctx
        .create(
            &mut node,
            "claude",
            "claude-sdk",
            json!({"grants": ["land"]}),
        )
        .await?;
    assert!(params.get("skillContext").is_none(), "{params}");

    let other = Ctx::boot().await?;
    let mut bare = FakeNode::connect(&other.hub, &other.host, true, None).await?;
    let (_, params) = other
        .create(&mut bare, "claude", "claude-sdk", json!({}))
        .await?;
    assert!(params.get("skillContext").is_none(), "{params}");
    Ok(())
}

/// Kinds `terminal` and `generic` run no agent that reads a skill or a first
/// input: nothing is stamped for them.
#[tokio::test]
async fn a_terminal_gets_no_stamp() -> Result<()> {
    let _switch = SWITCH.lock().await;
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, true, capable()).await?;
    let (_, params) = ctx
        .create(&mut node, "terminal", "shell-pty", json!({}))
        .await?;
    assert!(params.get("skillContext").is_none(), "{params}");
    Ok(())
}
