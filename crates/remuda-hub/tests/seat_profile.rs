//! cb-seat-profile (D-063, main-agent.md §3.4.2; protocol.md §8.4): the
//! lineage profile, its revision CAS and authority matrix, and its
//! application at the next chapter.
//!
//! Every case drives the Hub over HTTP with a scripted fake Node that records
//! the frames it receives: an edit never touches the live chapter, a resume
//! builds the successor spec from the profile at its current revision and
//! records it, the facts carry the model candidates, and nothing ever sends
//! an `instance.configure` (no automatic model switch).

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const SESSION: &str = "01993ab0-0000-7000-8000-0000000000cc";

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
        Self::connect_with(hub, host, herdr, &enroll, capabilities).await
    }

    /// Hello presenting `bearer` (an enroll token, or a prior hello's
    /// `nodeToken` to reconnect the same host).
    async fn connect_with(
        hub: &remuda_hub::RunningHub,
        host: &str,
        herdr: bool,
        enroll: &str,
        capabilities: Option<Value>,
    ) -> Result<Self> {
        let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {enroll}").parse()?);
        let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
        let mut inventory = json!({
            "hostname": "profile-node", "workspaces": [], "workspaceRevision": 0,
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
        let human = hub.mint_device_token("profile-owner").await?;
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

fn capable() -> Option<Value> {
    Some(json!({"memoryContext": true, "skillContext": true, "lineageFence": true}))
}

/// One HTTP call with any bearer and optional narrowing header.
async fn call(
    ctx: &Ctx,
    method: reqwest::Method,
    path: &str,
    token: &str,
    narrow: Option<&str>,
    body: Option<Value>,
) -> Result<(u16, Value)> {
    let mut request = ctx
        .http
        .request(method, format!("http://{}{path}", ctx.hub.addr))
        .bearer_auth(token);
    if let Some(instance) = narrow {
        request = request.header("x-remuda-instance-id", instance);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    let text = response.text().await?;
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(json!({"raw": text})),
    ))
}

async fn put(ctx: &Ctx, lineage: &str, token: &str, body: Value) -> Result<(u16, Value)> {
    call(
        ctx,
        reqwest::Method::PUT,
        &format!("/v1/lineages/{lineage}/profile"),
        token,
        None,
        Some(body),
    )
    .await
}

/// Seat a continuity chapter (a grant makes it one).
async fn seat(ctx: &Ctx, node: &mut FakeNode, extra: Value) -> Result<(String, Value)> {
    let mut body = json!({
        "grants": ["dispatch"],
        "model": "model-zero",
        "permissionMode": "manual",
        "effort": {"name": "low", "ultracode": false},
        "title": "Main",
    });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }
    ctx.create(node, "claude", "claude-sdk", body).await
}

/// Every frame the fake Node received so far, without waiting.
fn drain(node: &mut FakeNode) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    while let Ok(frame) = node.frames.try_recv() {
        out.push(frame);
    }
    out
}

fn assert_no_configure(frames: &[(String, Value)]) {
    for (method, params) in frames {
        assert!(
            !(method == "instance.command" && params.to_string().contains("instance.configure"))
                && method != "instance.configure",
            "no automatic model switch: {method} {params}"
        );
    }
}

#[tokio::test]
async fn the_profile_is_a_cas_document_and_a_stale_revision_gets_409() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, _) = seat(&ctx, &mut node, json!({})).await?;

    // Before the first edit: no document, nothing pending.
    let view = ctx.get(&format!("/v1/lineages/{lineage}/profile")).await?;
    assert_eq!(view["profile"], Value::Null, "{view}");
    assert_eq!(view["pending"], json!(false));
    assert_eq!(view["liveChapter"]["instanceId"], json!(lineage));
    assert_eq!(view["liveChapter"]["appliedRevision"], Value::Null);

    // The first edit creates revision 1 from the live chapter's spec.
    let (status, created) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 0, "effort": {"name": "high"}}),
    )
    .await?;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["profile"]["revision"], json!(1));
    assert_eq!(created["profile"]["modelCandidates"], json!(["model-zero"]));
    assert_eq!(created["profile"]["permissionMode"], json!("manual"));
    assert_eq!(created["profile"]["displayName"], json!("Main"));
    assert_eq!(
        created["profile"]["effort"],
        json!({"name": "high", "ultracode": false})
    );
    assert_eq!(created["changed"], json!(["effort"]));
    assert_eq!(created["pending"], json!(true), "the live chapter has none");

    // A second writer with the stale revision gets 409 and the current one.
    let (status, stale) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 0, "role": "late"}),
    )
    .await?;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["code"], json!("PROFILE_REVISION_CONFLICT"));
    assert_eq!(stale["currentRevision"], json!(1));
    assert_eq!(stale["reason"], json!("stale-revision"));
    assert_eq!(stale["next"]["action"], json!("read-then-decide"));

    let (status, second) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 1, "modelCandidates": ["model-a", "model-b"],
               "role": "keeps the loop running"}),
    )
    .await?;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["profile"]["revision"], json!(2));
    assert_eq!(second["changed"], json!(["role", "modelCandidates"]));
    assert_eq!(
        second["profile"]["effort"],
        json!({"name": "high", "ultracode": false}),
        "an absent field keeps its value"
    );

    // Audit: who, the revision, the changed field names only.
    let audit = ctx
        .hub
        .store()
        .context("hub store")?
        .audit_for(lineage.clone())
        .await?;
    let rows: Vec<&Value> = audit
        .iter()
        .filter(|row| row["action"] == json!("lineage.profile.set"))
        .collect();
    assert_eq!(rows.len(), 2, "{audit:?}");
    assert_eq!(
        rows[1]["detail"],
        json!({"revision": 2, "changed": ["role", "modelCandidates"]})
    );
    let text = serde_json::to_string(&rows).unwrap();
    assert!(
        !text.contains("model-a") && !text.contains("keeps the loop"),
        "{text}"
    );
    Ok(())
}

#[tokio::test]
async fn only_an_unnarrowed_human_edits_and_the_seat_reads_its_own() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, params) = seat(&ctx, &mut node, json!({})).await?;
    let seat_token = params["agentCredential"]["token"]
        .as_str()
        .context("seat credential")?
        .to_owned();
    let (other, other_params) = ctx
        .create(
            &mut node,
            "claude",
            "claude-sdk",
            json!({"grants": ["land"]}),
        )
        .await?;
    let other_token = other_params["agentCredential"]["token"]
        .as_str()
        .context("other credential")?
        .to_owned();
    let bot = ctx.hub.mint_bot_device_token("profile-bot").await?;
    let path = format!("/v1/lineages/{lineage}/profile");
    let edit = json!({"expectedRevision": 0, "permissionMode": "bypassPermissions"});

    // The seat itself may never edit (it would raise its own mode).
    let (status, body) = put(&ctx, &lineage, &seat_token, edit.clone()).await?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["reason"], json!("profile-human-only"), "{body}");
    assert_eq!(body["next"]["action"], json!("ask-owner"));
    // Nor another Agent, a Bot, or a Human token narrowed to the seat.
    let (status, body) = put(&ctx, &lineage, &other_token, edit.clone()).await?;
    assert_eq!(status, 403, "{body}");
    let (status, body) = put(&ctx, &lineage, &bot, edit.clone()).await?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["reason"], json!("profile-human-only"), "{body}");
    let (status, body) = call(
        &ctx,
        reqwest::Method::PUT,
        &path,
        &ctx.human,
        Some(&lineage),
        Some(edit.clone()),
    )
    .await?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["reason"], json!("profile-human-only"), "{body}");
    let view = ctx.get(&path).await?;
    assert_eq!(view["profile"], Value::Null, "nothing was written: {view}");

    // The owner may.
    let (status, body) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 0, "modelCandidates": ["model-a", "model-b"]}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // Reads: the seat reads its own; another seat, and a Bot, may not.
    let (status, own) = call(&ctx, reqwest::Method::GET, &path, &seat_token, None, None).await?;
    assert_eq!(status, 200, "{own}");
    assert_eq!(
        own["profile"]["modelCandidates"],
        json!(["model-a", "model-b"])
    );
    let (status, body) = call(&ctx, reqwest::Method::GET, &path, &other_token, None, None).await?;
    assert_eq!(status, 403, "{body}");
    let (status, body) = call(&ctx, reqwest::Method::GET, &path, &bot, None, None).await?;
    assert_eq!(status, 403, "{body}");
    let _ = other;
    Ok(())
}

#[tokio::test]
async fn the_bounds_are_enforced_before_anything_is_stored() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, _) = seat(&ctx, &mut node, json!({})).await?;
    for (body, needle) in [
        (
            json!({"expectedRevision": 0, "modelCandidates": []}),
            "1 to 5",
        ),
        (
            json!({"expectedRevision": 0, "modelCandidates": ["a", "b", "c", "d", "e", "f"]}),
            "1 to 5",
        ),
        (
            json!({"expectedRevision": 0, "modelCandidates": ["a", "a"]}),
            "twice",
        ),
        (
            json!({"expectedRevision": 0, "permissionMode": "never"}),
            "not a claude mode",
        ),
        (
            json!({"expectedRevision": 0, "skills": ["relative/kit"]}),
            "host-absolute",
        ),
        (
            json!({"expectedRevision": 0, "skills": ["/x/remuda"]}),
            "host-absolute",
        ),
        (json!({"modelCandidates": ["a"]}), "expectedRevision"),
    ] {
        let (status, refused) = put(&ctx, &lineage, &ctx.human, body.clone()).await?;
        assert_eq!(status, 400, "{body} → {refused}");
        assert!(refused.to_string().contains(needle), "{body} → {refused}");
    }
    let (status, secret) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 0,
               "role": "sk-ant-api03-SEATPROFxSECRETxVALUEx0123456789abcdefABCDEF0123456789"}),
    )
    .await?;
    assert_eq!(status, 422, "{secret}");
    assert!(
        !secret.to_string().contains("SECRETxVALUE"),
        "never echoed: {secret}"
    );
    let five = json!({"expectedRevision": 0, "modelCandidates": ["a", "b", "c", "d", "e"]});
    let (status, body) = put(&ctx, &lineage, &ctx.human, five).await?;
    assert_eq!(status, 200, "five is the bound: {body}");
    Ok(())
}

#[tokio::test]
async fn the_next_chapter_applies_the_profile_and_records_the_revision() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, first) = seat(&ctx, &mut node, json!({})).await?;
    assert!(
        first["skillContext"].get("profile").is_none(),
        "the first chapter launched without a profile: {first}"
    );
    let (status, body) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 0, "modelCandidates": ["model-a", "model-b"],
               "effort": {"name": "high"}, "permissionMode": "acceptEdits",
               "displayName": "Main", "role": "keeps the loop running",
               "skills": ["/srv/skills/review-kit"]}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");

    // A mid-chapter edit changes nothing live: the chapter's record keeps its
    // spec, it applied no revision, and the Node got no frame.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let live = ctx.get(&format!("/v1/instances/{lineage}")).await?;
    assert_eq!(live["model"], json!("model-zero"), "{live}");
    assert_eq!(live["permissionMode"], json!("manual"), "{live}");
    assert_eq!(live["effortName"], json!("low"), "{live}");
    assert!(live.get("profileRevision").is_none(), "{live}");
    let frames = drain(&mut node);
    assert!(
        frames.is_empty(),
        "an edit sends the live chapter nothing: {frames:?}"
    );
    let view = ctx.get(&format!("/v1/lineages/{lineage}/profile")).await?;
    assert_eq!(view["pending"], json!(true), "{view}");

    // The next chapter: an owner Resume after the chapter ended.
    ctx.end(&node, &lineage).await?;
    let resumed = ctx
        .ok(&format!("/v1/instances/{lineage}/resume"), json!({}))
        .await?;
    let successor = resumed["instance"]["instanceId"]
        .as_str()
        .context("successor")?
        .to_owned();
    let params = node.next("instance.resume").await?;
    let spec = &params["spec"];
    assert_eq!(
        spec["model"],
        json!("model-a"),
        "the first candidate: {spec}"
    );
    assert_eq!(
        spec["effort"],
        json!({"name": "high", "ultracode": false}),
        "{spec}"
    );
    assert_eq!(spec["effortName"], json!("high"), "{spec}");
    assert_eq!(spec["permissionMode"], json!("acceptEdits"), "{spec}");
    assert_eq!(spec["skills"], json!(["/srv/skills/review-kit"]), "{spec}");
    // The facts carry the applied revision and the candidates, as a fact.
    assert_eq!(
        params["skillContext"]["profile"],
        json!({"revision": "1", "displayName": "Main", "role": "keeps the loop running",
               "modelCandidates": ["model-a", "model-b"]}),
        "{params}"
    );
    // The successor records it; the badge clears.
    let row = ctx.get(&format!("/v1/instances/{successor}")).await?;
    assert_eq!(row["profileRevision"], json!(1), "{row}");
    let view = ctx.get(&format!("/v1/lineages/{lineage}/profile")).await?;
    assert_eq!(view["liveChapter"]["instanceId"], json!(successor));
    assert_eq!(view["liveChapter"]["appliedRevision"], json!(1));
    assert_eq!(view["pending"], json!(false), "{view}");

    // Edit again: pending until the chapter after.
    let (status, _) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 1, "modelCandidates": ["model-b"]}),
    )
    .await?;
    assert_eq!(status, 200);
    let view = ctx.get(&format!("/v1/lineages/{lineage}/profile")).await?;
    assert_eq!(view["pending"], json!(true), "{view}");

    // Nothing ever switched a model on its own.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_no_configure(&drain(&mut node));
    Ok(())
}

#[tokio::test]
async fn without_a_profile_the_next_chapter_is_unchanged() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, _) = seat(&ctx, &mut node, json!({"skills": ["/srv/skills/kept-kit"]})).await?;
    ctx.end(&node, &lineage).await?;
    let resumed = ctx
        .ok(&format!("/v1/instances/{lineage}/resume"), json!({}))
        .await?;
    let successor = resumed["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let params = node.next("instance.resume").await?;
    let spec = &params["spec"];
    assert_eq!(spec["model"], json!("model-zero"), "{spec}");
    assert_eq!(spec["effortName"], json!("low"), "{spec}");
    assert_eq!(spec["permissionMode"], json!("manual"), "{spec}");
    assert_eq!(
        spec["skills"],
        json!(["/srv/skills/kept-kit"]),
        "create-time skills carry over like the rest of the spec: {spec}"
    );
    assert!(params["skillContext"].get("profile").is_none(), "{params}");
    let row = ctx.get(&format!("/v1/instances/{successor}")).await?;
    assert!(row.get("profileRevision").is_none(), "{row}");
    Ok(())
}

/// A profile that changes the mode must not hand the successor the
/// predecessor's observed effective mode (OA1 reads it for the seat's
/// children).
#[tokio::test]
async fn a_changed_mode_does_not_carry_the_old_effective_mode() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, _) = seat(
        &ctx,
        &mut node,
        json!({"permissionMode": "bypassPermissions"}),
    )
    .await?;
    node.appends.send((
        lineage.clone(),
        json!({"kind": "permission", "payload": {
            "requested": "bypassPermissions",
            "effective": {"mode": "bypassPermissions", "source": "transcript"},
            "raw": "bypassPermissions"
        }}),
    ))?;
    let (status, body) = put(
        &ctx,
        &lineage,
        &ctx.human,
        json!({"expectedRevision": 0, "permissionMode": "manual"}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    ctx.end(&node, &lineage).await?;
    ctx.ok(&format!("/v1/instances/{lineage}/resume"), json!({}))
        .await?;
    let params = node.next("instance.resume").await?;
    assert_eq!(params["spec"]["permissionMode"], json!("manual"));
    assert!(
        params["spec"]
            .get("permissionEffective")
            .is_none_or(|value| value["mode"] != json!("bypassPermissions")),
        "{params}"
    );
    Ok(())
}

#[tokio::test]
async fn the_launch_snapshot_shows_on_the_instance_and_the_profile_read() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, _) = seat(&ctx, &mut node, json!({})).await?;
    let payload = json!({
        "launch": "create", "harness": "claude", "driver": "claude-sdk",
        "profileRevision": null,
        "configured": [{"class": "skill", "name": "locked-kit", "source": "spec"}],
        "resolved": [], "skipped": [],
        "unavailable": [{"class": "skill", "name": "locked-kit", "reason": "unreadable"}],
        "refusal": {"reason": "launch-capability-unreadable",
                    "next": {"action": "ask-owner", "hint": "make it readable"}}
    });
    node.appends.send((
        lineage.clone(),
        json!({"kind": "launch.capabilities", "payload": payload}),
    ))?;
    let row = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = ctx.get(&format!("/v1/instances/{lineage}")).await?;
            if row.get("launchCapabilities").is_some() {
                return anyhow::Ok(row);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    assert_eq!(row["launchCapabilities"], payload);
    let view = ctx.get(&format!("/v1/lineages/{lineage}/profile")).await?;
    assert_eq!(view["lastLaunch"]["instanceId"], json!(lineage));
    assert_eq!(
        view["lastLaunch"]["payload"]["unavailable"][0]["reason"],
        json!("unreadable")
    );
    Ok(())
}

#[tokio::test]
async fn an_agent_create_cannot_configure_skills() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (_, params) = seat(&ctx, &mut node, json!({})).await?;
    let token = params["agentCredential"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = call(
        &ctx,
        reqwest::Method::POST,
        "/v1/instances",
        &token,
        None,
        Some(
            json!({"hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
                    "prompt": "child", "skills": ["/srv/skills/kit"]}),
        ),
    )
    .await?;
    assert_eq!(status, 403, "{body}");
    assert!(body.to_string().contains("skills"), "{body}");
    // A Human create with an invalid list is a 400 before anything is stored.
    let (status, body) = call(
        &ctx,
        reqwest::Method::POST,
        "/v1/instances",
        &ctx.human,
        None,
        Some(
            json!({"hostId": ctx.host, "kind": "claude", "driver": "claude-sdk",
                    "prompt": "x", "skills": ["not/absolute"]}),
        ),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    Ok(())
}

/// A launch the Node refused never started: its entity names the instance's
/// own id as a placeholder session. An owner Resume after the fix (main-agent
/// §3.4.2) must launch fresh from the origin spec, never `--resume` that id.
#[tokio::test]
async fn a_refused_launch_resumes_as_a_fresh_create_not_a_bogus_session() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let mut node = FakeNode::connect(&ctx.hub, &ctx.host, false, capable()).await?;
    let (lineage, _) = seat(
        &ctx,
        &mut node,
        json!({"skills": ["/srv/skills/locked-kit"]}),
    )
    .await?;
    node.appends.send((
        lineage.clone(),
        json!({"kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "entityId": lineage,
            "revision": "3", "previousState": "preparing", "state": "failed",
            "reasonCode": "launch refused (launch-capability-unreadable): …",
            "evidenceEventIds": [],
            "entity": {"id": lineage, "nativeRef": {"sessionId": {"state": "known", "value": lineage}}}
        }}),
    ))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = ctx.get(&format!("/v1/instances/{lineage}")).await?;
            if view["lifecycle"] == json!("failed") {
                return anyhow::Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    ctx.ok(&format!("/v1/instances/{lineage}/resume"), json!({}))
        .await?;
    let params = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (method, params) = node.frames.recv().await.context("node closed")?;
            if method == "instance.create" || method == "instance.resume" {
                return anyhow::Ok((method, params));
            }
        }
    })
    .await??;
    assert_eq!(params.0, "instance.create", "{params:?}");
    assert!(
        params.1["spec"].get("resumeSessionId").is_none(),
        "{params:?}"
    );
    assert_eq!(
        params.1["spec"]["skills"],
        json!(["/srv/skills/locked-kit"])
    );
    Ok(())
}
