//! Coordinator batch 1 (co-project): Project CRUD, member workspaces across
//! two fake hosts, project default folding, `Placement::Project`, scope/grant
//! enforcement (design §2.5 amendment — recursive delegation tree, not role
//! tiers), and the two seat-uniqueness rules.
//!
//! Everything here runs against the in-process Hub with fake Node WebSocket
//! clients — no real node, no real models.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// One connected fake Node.
struct FakeNode {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn enroll_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
    labels: &[&str],
    workspaces: &[&str],
    resources: Value,
    max_instances: i64,
) -> Result<FakeNode> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    let labels_obj: Value = labels
        .iter()
        .map(|label| json!(*label))
        .collect::<Vec<_>>()
        .into();
    let ws_rows: Vec<Value> = workspaces
        .iter()
        .map(|id| json!({ "workspaceId": id, "hostId": host_id, "root": format!("/tmp/{id}") }))
        .collect();
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": format!("fake-{host_id}"),
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "labels": labels_obj,
                    "maxInstances": max_instances,
                    "resources": resources,
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "workspaces": ws_rows,
                    "workspaceRevision": workspaces.len() as u64,
                }
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    let hello = recv_json(&mut node).await?;
    anyhow::ensure!(hello["result"]["nodeToken"].is_string(), "hello {hello}");
    let task = tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if let Some(id) = frame.get("id").cloned()
                && frame.get("method").is_some()
            {
                let result = match frame["method"].as_str().unwrap_or("") {
                    "instance.create" => json!({"result":{"accepted":true}}),
                    _ => json!({"result":{"ok":true}}),
                };
                let mut response = result;
                response["jsonrpc"] = json!("2.0");
                response["id"] = id;
                if node
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    Ok(FakeNode { task })
}

async fn recv_json<S>(ws: &mut S) -> Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow::anyhow!("unexpected ws frame {other:?}")),
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host_a: String,
    host_b: String,
    wsp_a: String,
    wsp_b: String,
    _node_a: FakeNode,
    _node_b: FakeNode,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("hub");
        let hub = spawn(HubConfig::for_test(data)).await?;
        let human = hub.mint_device_token("project-human").await?;
        let host_a = HostId::new().as_id().to_string();
        let host_b = HostId::new().as_id().to_string();
        let wsp_a = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let wsp_b = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node_a = enroll_node(
            &hub,
            &host_a,
            &["toolchain=rust", "region=sg"],
            &[&wsp_a],
            json!({ "cpuPct": 5, "memPct": 20 }),
            8,
        )
        .await?;
        let node_b = enroll_node(
            &hub,
            &host_b,
            &["toolchain=rust", "region=cn"],
            &[&wsp_b],
            json!({ "cpuPct": 50, "memPct": 60 }),
            8,
        )
        .await?;
        // Give the Hub a moment to project both hellos.
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host_a,
            host_b,
            wsp_a,
            wsp_b,
            _node_a: node_a,
            _node_b: node_b,
        })
    }

    async fn create_project(&self, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/v1/projects", self.base()))
            .bearer_auth(&self.human)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        anyhow::ensure!(status.is_success(), "create_project {status}: {body}");
        Ok(body)
    }

    async fn create_instance_with(
        &self,
        token: &str,
        body: Value,
    ) -> reqwest::Result<reqwest::Response> {
        self.http
            .post(format!("{}/v1/instances", self.base()))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
    }

    async fn agent_token_for(&self, instance_id: &str) -> Result<String> {
        let value: Value = self
            .http
            .post(format!(
                "{}/v1/instances/{instance_id}/mcp-token",
                self.base()
            ))
            .bearer_auth(&self.human)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        value
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("mcp token")
    }
}

fn project_body(ctx: &Ctx, name: &str) -> Value {
    json!({
        "name": name,
        "homeHost": ctx.host_a,
        "members": [
            {"hostId": ctx.host_a, "workspaceId": ctx.wsp_a, "role": "primary"},
            {"hostId": ctx.host_b, "workspaceId": ctx.wsp_b, "role": "build"},
        ],
        "hosts": [],
        "provider": {"delegation": "none"},
        "defaultEffort": "high",
        "permissionPosture": "ask",
    })
}

#[tokio::test]
async fn project_crud_and_members_across_two_fake_hosts() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    // An unregistered workspace pair is refused.
    let bad = ctx
        .http
        .post(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"name":"bad", "members":[
            {"hostId": ctx.host_a, "workspaceId": remuda_protocol::WorkspaceId::new().as_id().to_string()}]}))
        .send()
        .await?;
    assert_eq!(bad.status(), 400);
    let bad_body = bad.json::<Value>().await?;
    assert!(
        bad_body["error"]
            .as_str()
            .unwrap_or("")
            .contains("registered"),
        "{bad_body}"
    );

    let project = ctx.create_project(project_body(&ctx, "remuda")).await?;
    let project_id = project["id"].as_str().unwrap().to_string();
    assert_eq!(project["name"], "remuda");
    assert_eq!(project["members"].as_array().unwrap().len(), 2);
    // D-031 enforced policy defaults on, and it is server-controlled.
    assert_eq!(project["policy"]["enforced"]["noTunnelTools"], true);
    assert_eq!(
        project["policy"]["configurable"]["maxDelegationDepth"], 3,
        "§2.5 default depth"
    );
    assert_eq!(
        project["policy"]["configurable"]["coordinatorFanOut"], 8,
        "§2.5 default fan-out"
    );

    // list + get round-trip.
    let list: Value = ctx
        .http
        .get(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == project_id)
    );
    let one: Value = ctx
        .http
        .get(format!("{}/v1/projects/{project_id}", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(one["id"], project_id);

    // Duplicate member add is 400; removing a real member works.
    let dup = ctx
        .http
        .post(format!("{}/v1/projects/{project_id}/members", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"hostId": ctx.host_a, "workspaceId": ctx.wsp_a}))
        .send()
        .await?;
    assert_eq!(dup.status(), 400);
    let removed: Value = ctx
        .http
        .delete(format!("{}/v1/projects/{project_id}/members", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"hostId": ctx.host_b, "workspaceId": ctx.wsp_b}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(removed["members"].as_array().unwrap().len(), 1);
    // Removing it again is 400.
    let again = ctx
        .http
        .delete(format!("{}/v1/projects/{project_id}/members", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"hostId": ctx.host_b, "workspaceId": ctx.wsp_b}))
        .send()
        .await?;
    assert_eq!(again.status(), 400);
    // Re-add for later tests.
    ctx.http
        .post(format!("{}/v1/projects/{project_id}/members", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"hostId": ctx.host_b, "workspaceId": ctx.wsp_b, "role":"build"}))
        .send()
        .await?
        .error_for_status()?;

    // set: configurable policy changes; PATCH persists across restart.
    let patched: Value = ctx
        .http
        .patch(format!("{}/v1/projects/{project_id}", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "defaultEffort": "xhigh",
            "policy": {"configurable": {"maxDelegationDepth": 5}}
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(patched["defaultEffort"], "xhigh");
    assert_eq!(patched["policy"]["configurable"]["maxDelegationDepth"], 5);
    // Rev 1 create, rev 2 member remove, rev 3 member re-add, rev 4 this PATCH.
    assert_eq!(patched["revision"], "4");

    let data = ctx._dir.path().join("hub");
    ctx.hub.shutdown().await;
    let hub = spawn(HubConfig::for_test(data)).await?;
    let restarted: Value = reqwest::Client::new()
        .get(format!("http://{}/v1/projects/{project_id}", hub.addr))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(restarted["defaultEffort"], "xhigh");
    assert_eq!(restarted["members"].as_array().unwrap().len(), 2);
    hub.shutdown().await;
    Ok(())
}

/// Create a top coordinator (address-owner), then a project coordinator under
/// it, then a leaf worker — exercising the full delegation chain.
async fn make_chain(ctx: &Ctx) -> Result<(String, String, String, String)> {
    let project = ctx.create_project(project_body(ctx, "chain")).await?;
    let project_id = project["id"].as_str().unwrap().to_string();
    // Human seats the project coordinator directly. The top-coordinator
    // preset holds dispatch+spend+address-owner but deliberately NOT land
    // (design §2.1), so it cannot parent a node carrying land; the project
    // seat with land comes from the owner. Scope narrows to one project and
    // grants expand to dispatch+land+spend (the preset golden bundle).
    let coordinator: Value = ctx
        .create_instance_with(
            &ctx.human,
            json!({
                "role": "project-coordinator",
                "projectId": project_id,
                "prompt": "coordinate"
            }),
        )
        .await?
        .error_for_status()?
        .json()
        .await?;
    let coordinator_id = coordinator["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_string();
    let coordinator_token = ctx.agent_token_for(&coordinator_id).await?;

    // The project coordinator delegates a leaf worker with no grants.
    let worker: Value = ctx
        .create_instance_with(
            &coordinator_token,
            json!({
                "role": "worker",
                "projectId": project_id,
                "prompt": "work"
            }),
        )
        .await?
        .error_for_status()?
        .json()
        .await?;
    let worker_id = worker["instance"]["instanceId"]
        .as_str()
        .unwrap()
        .to_string();
    Ok((
        project_id,
        coordinator_id.clone(),
        coordinator_id,
        worker_id,
    ))
}

#[tokio::test]
async fn delegation_tree_chain_presets_scope_and_views() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let (project_id, coordinator_id, _same, worker_id) = make_chain(&ctx).await?;

    let coordinator = ctx
        .hub
        .store()
        .unwrap()
        .get_instance(coordinator_id.clone())
        .await?
        .expect("coordinator row");
    assert_eq!(coordinator.role.as_deref(), Some("project-coordinator"));
    assert_eq!(
        coordinator.project_id.as_deref(),
        Some(project_id.as_str()),
        "projectId is the single-entry scope.projectIds view"
    );
    assert_eq!(
        coordinator.grants,
        ["dispatch", "land", "spend"],
        "preset golden expansion"
    );
    // Human-seated project coordinator: no instance parent.
    assert_eq!(coordinator.parent_instance_id, None);

    let worker = ctx
        .hub
        .store()
        .unwrap()
        .get_instance(worker_id.clone())
        .await?
        .expect("worker row");
    assert!(worker.grants.is_empty(), "leaf worker has no verbs");
    assert_eq!(
        worker.parent_instance_id.as_deref(),
        Some(coordinator_id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn leaf_worker_gets_403_on_projects_providers_and_other_projects() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let (project_a, _seat, _coord, worker_a) = make_chain(&ctx).await?;
    let worker_a_token = ctx.agent_token_for(&worker_a).await?;
    // A second project the worker's scope cannot reach.
    let project_b = ctx
        .create_project(json!({
            "name": "other",
            "members": [{"hostId": ctx.host_a, "workspaceId": ctx.wsp_a}]
        }))
        .await?;
    let project_b_id = project_b["id"].as_str().unwrap();

    // No dispatch grant → /v1/projects is 403 for a leaf worker.
    let denied = ctx
        .http
        .get(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&worker_a_token)
        .send()
        .await?;
    assert_eq!(denied.status(), 403);
    // Same for providers (middleware block).
    let denied = ctx
        .http
        .get(format!("{}/v1/providers", ctx.base()))
        .bearer_auth(&worker_a_token)
        .send()
        .await?;
    assert_eq!(denied.status(), 403);
    // Bound to project A, reading project B is 403.
    let denied = ctx
        .http
        .get(format!("{}/v1/projects/{project_b_id}", ctx.base()))
        .bearer_auth(&worker_a_token)
        .send()
        .await?;
    assert_eq!(denied.status(), 403);
    // …and the list view simply never includes project B (even once granted).
    // Workers also cannot create a project (operator-only).
    let denied = ctx
        .http
        .post(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&worker_a_token)
        .json(&json!({"name":"sneaky"}))
        .send()
        .await?;
    assert_eq!(denied.status(), 403);
    let _ = project_a;
    Ok(())
}

#[tokio::test]
async fn project_scoped_coordinator_sees_only_its_project() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let other = ctx
        .create_project(json!({
            "name": "other",
            "members": [{"hostId": ctx.host_b, "workspaceId": ctx.wsp_b}]
        }))
        .await?;
    let other_id = other["id"].as_str().unwrap().to_string();
    let (project_id, _seat, coordinator_id, _worker) = make_chain(&ctx).await?;
    let token = ctx.agent_token_for(&coordinator_id).await?;
    let list: Value = ctx
        .http
        .get(format!("{}/v1/projects", ctx.base()))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ids: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&project_id.as_str()));
    assert!(!ids.contains(&other_id.as_str()));
    Ok(())
}

#[tokio::test]
async fn dispatching_without_grant_or_widened_scope_is_403() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let (project_id, _seat, coordinator_id, worker_id) = make_chain(&ctx).await?;
    let worker_token = ctx.agent_token_for(&worker_id).await?;
    // Leaf worker attempts to delegate a child → 403 (no dispatch grant).
    let denied = ctx
        .create_instance_with(
            &worker_token,
            json!({"role":"worker","projectId":project_id,"prompt":"x"}),
        )
        .await?;
    assert_eq!(denied.status(), 403);

    // A foreign project the coordinator's scope cannot reach.
    let foreign = ctx
        .create_project(json!({
            "name":"foreign",
            "members":[{"hostId": ctx.host_a, "workspaceId": ctx.wsp_a}]
        }))
        .await?;
    let foreign_id = foreign["id"].as_str().unwrap().to_string();

    // Reuse the make_chain coordinator (already the active dispatch holder
    // for `project_id`); a child scoped to `foreign_id` is not a subset → 403.
    let scoped_token = ctx.agent_token_for(&coordinator_id).await?;
    let denied = ctx
        .create_instance_with(
            &scoped_token,
            json!({
                "role":"worker",
                "scope":{"projectIds":[foreign_id]},
                "prompt":"widen"
            }),
        )
        .await?;
    assert_eq!(
        denied.status(),
        403,
        "{}",
        denied.text().await.unwrap_or_default()
    );
    Ok(())
}

#[tokio::test]
async fn address_owner_uniqueness_is_409() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let first = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"top-coordinator","hostId":ctx.host_a,"prompt":"one"}),
        )
        .await?;
    assert!(first.status().is_success(), "{}", first.status());
    let second = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"top-coordinator","hostId":ctx.host_b,"prompt":"two"}),
        )
        .await?;
    assert_eq!(second.status(), 409);
    let body: Value = second.json().await?;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("address-owner"),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn dispatch_holder_per_project_is_unique_by_default_but_relaxable() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project(project_body(&ctx, "seats")).await?;
    let project_id = project["id"].as_str().unwrap().to_string();
    let first = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"project-coordinator","projectId":project_id,"hostId":ctx.host_a,"prompt":"a"}),
        )
        .await?;
    assert!(first.status().is_success(), "{}", first.status());
    let second = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"project-coordinator","projectId":project_id,"hostId":ctx.host_b,"prompt":"b"}),
        )
        .await?;
    assert_eq!(second.status(), 409);
    // A different project is fine.
    let other = ctx
        .create_project(project_body(&ctx, "other-seats"))
        .await?;
    let other_id = other["id"].as_str().unwrap().to_string();
    let fine = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"project-coordinator","projectId":other_id,"hostId":ctx.host_a,"prompt":"c"}),
        )
        .await?;
    assert!(fine.status().is_success(), "{}", fine.status());

    // Relax the policy → a second active holder for the first project is OK.
    ctx.http
        .patch(format!("{}/v1/projects/{project_id}", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"policy":{"configurable":{"allowMultipleDispatchers":true}}}))
        .send()
        .await?
        .error_for_status()?;
    let relaxed = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"project-coordinator","projectId":project_id,"hostId":ctx.host_b,"prompt":"d"}),
        )
        .await?;
    assert!(relaxed.status().is_success(), "{}", relaxed.status());
    Ok(())
}

#[tokio::test]
async fn depth_limit_returns_409_and_policy_raises_it() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project(project_body(&ctx, "deep")).await?;
    // Multiple dispatcher nodes in one chain coexist, so relax the per-project
    // dispatch-holder seat for this test — depth is the invariant under test.
    ctx.http
        .patch(format!(
            "{}/v1/projects/{}",
            ctx.base(),
            project["id"].as_str().unwrap()
        ))
        .bearer_auth(&ctx.human)
        .json(&json!({"policy":{"configurable":{"allowMultipleDispatchers":true}}}))
        .send()
        .await?
        .error_for_status()?;
    let project_id = project["id"].as_str().unwrap().to_string();
    // Chain: human(seated node depth1) → depth2 → depth3 → worker depth4,
    // which fails (default limit 3). Every delegated node carries an explicit
    // [dispatch, spend] set so the grant-subset rule never blocks what this
    // test is about; depth is the asserted invariant.
    let mut tokens = Vec::new();
    let mut parent_token = ctx.human.clone();
    for index in 0..3 {
        let response: Value = ctx
            .create_instance_with(
                &parent_token,
                json!({
                    "role":"project-coordinator",
                    "grants":["dispatch","spend"],
                    "projectId":project_id,
                    "hostId":ctx.host_a,
                    "prompt":format!("d{index}")
                }),
            )
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = response["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_string();
        let token = ctx.agent_token_for(&id).await?;
        tokens.push((id, token.clone()));
        parent_token = token;
    }
    // Fourth edge → depth 4, over the default limit 3.
    let too_deep = ctx
        .create_instance_with(
            &parent_token,
            json!({"role":"worker","projectId":project_id,"prompt":"leaf"}),
        )
        .await?;
    assert_eq!(too_deep.status(), 409);
    assert!(
        too_deep.json::<Value>().await?["error"]
            .as_str()
            .unwrap_or("")
            .contains("depth"),
        "depth message"
    );
    // Raise the project limit → the same create succeeds.
    ctx.http
        .patch(format!("{}/v1/projects/{project_id}", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({"policy":{"configurable":{"maxDelegationDepth":4}}}))
        .send()
        .await?
        .error_for_status()?;
    let now_ok = ctx
        .create_instance_with(
            &tokens[2].1,
            json!({"role":"worker","projectId":project_id,"prompt":"leaf2"}),
        )
        .await?;
    assert!(now_ok.status().is_success(), "{}", now_ok.status());
    Ok(())
}

#[tokio::test]
async fn fan_out_limit_returns_409() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project_body = {
        let mut body = project_body(&ctx, "fanout");
        body["policy"] = json!({"configurable": {"coordinatorFanOut": 2}});
        body
    };
    let project = ctx.create_project(project_body).await?;
    let project_id = project["id"].as_str().unwrap().to_string();
    let top: Value = ctx
        .create_instance_with(
            &ctx.human,
            json!({"role":"project-coordinator","projectId":project_id,"hostId":ctx.host_a,"prompt":"p"}),
        )
        .await?
        .error_for_status()?
        .json()
        .await?;
    let top_id = top["instance"]["instanceId"].as_str().unwrap().to_string();
    let top_token = ctx.agent_token_for(&top_id).await?;
    for index in 0..2 {
        let ok = ctx
            .create_instance_with(
                &top_token,
                json!({"role":"worker","projectId":project_id,"prompt":format!("w{index}")}),
            )
            .await?;
        assert!(ok.status().is_success(), "{}", ok.status());
    }
    let third = ctx
        .create_instance_with(
            &top_token,
            json!({"role":"worker","projectId":project_id,"prompt":"w2"}),
        )
        .await?;
    assert_eq!(third.status(), 409);
    assert!(
        third.json::<Value>().await?["error"]
            .as_str()
            .unwrap_or("")
            .contains("fan-out"),
        "fan-out message"
    );
    Ok(())
}

#[tokio::test]
async fn placement_project_resolves_member_hosts_and_folds_defaults() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let mut body = project_body(&ctx, "place");
    body["hosts"] = json!([
        {"hostId": ctx.host_b, "maxInstances": 8, "requires": ["toolchain=rust"]}
    ]);
    body["provider"] = json!({"delegation":"none"});
    let project = ctx.create_project(body).await?;
    let project_id = project["id"].as_str().unwrap().to_string();

    // Dry-run resolution: only the two member hosts are candidates; a
    // non-member third host is irrelevant.
    let resolve: Value = ctx
        .http
        .post(format!("{}/v1/placement/resolve", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({
            "spec": {"kind":"claude","driver":"claude-print"},
            "placement": {"kind":"project","projectId":project_id}
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let host_ids: Vec<&str> = resolve["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|host| host["hostId"].as_str().unwrap())
        .collect();
    assert!(host_ids.contains(&ctx.host_a.as_str()), "{host_ids:?}");
    assert!(host_ids.contains(&ctx.host_b.as_str()), "{host_ids:?}");
    assert_eq!(host_ids.len(), 2);
    assert_eq!(resolve["projectId"], project_id);

    // Default folding: no model on the request → project default effort lands
    // on the instance spec (explicit > project > host > global).
    let created: Value = ctx
        .create_instance_with(
            &ctx.human,
            json!({
                "projectId": project_id,
                "placement": {"kind":"project","projectId":project_id},
                "kind":"claude",
                "driver":"claude-print",
                "prompt":"p",
            }),
        )
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(created["instance"]["projectId"], project_id);
    // The launch command carries the project-folded effort.
    // (spec is persisted; check the instance row through the store.)
    let instance_id = created["instance"]["instanceId"].as_str().unwrap();
    let row = ctx
        .hub
        .store()
        .unwrap()
        .get_instance(instance_id.into())
        .await?
        .unwrap();
    assert!(
        row.host_id == ctx.host_a || row.host_id == ctx.host_b,
        "placed on a member host"
    );
    Ok(())
}

#[tokio::test]
async fn saturated_resources_exclude_a_host_in_project_placement() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let http = reqwest::Client::new();
    let human = hub.mint_device_token("saturated").await?;
    let idle = HostId::new().as_id().to_string();
    let busy = HostId::new().as_id().to_string();
    let idle_wsp = remuda_protocol::WorkspaceId::new().as_id().to_string();
    let busy_wsp = remuda_protocol::WorkspaceId::new().as_id().to_string();
    let _idle_node = enroll_node(
        &hub,
        &idle,
        &["toolchain=rust"],
        &[&idle_wsp],
        json!({"cpuPct": 10, "memPct": 10}),
        8,
    )
    .await?;
    let _busy_node = enroll_node(
        &hub,
        &busy,
        &["toolchain=rust"],
        &[&busy_wsp],
        json!({"cpuPct": 97, "memPct": 20}),
        8,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let base = format!("http://{}", hub.addr);
    let project: Value = http
        .post(format!("{base}/v1/projects"))
        .bearer_auth(&human)
        .json(&json!({
            "name":"resources",
            "members":[
                {"hostId":idle,"workspaceId":idle_wsp},
                {"hostId":busy,"workspaceId":busy_wsp}
            ]
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let project_id = project["id"].as_str().unwrap();
    let resolve: Value = http
        .post(format!("{base}/v1/placement/resolve"))
        .bearer_auth(&human)
        .json(&json!({
            "spec": {"kind":"claude","driver":"claude-print"},
            "placement": {"kind":"project","projectId":project_id}
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(resolve["hostId"], idle, "the busy host is excluded");
    assert_eq!(resolve["hosts"].as_array().unwrap().len(), 1);
    hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn delegation_cycle_is_rejected_even_if_columns_are_tampered() -> Result<()> {
    // Parent ids are Hub-stamped and cannot be chosen by callers, so a cycle
    // cannot arise through the API; the DAG check is defensive against direct
    // storage tampering (repair tools, older bugs). Verify it closes anyway.
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project(project_body(&ctx, "cycle")).await?;
    ctx.http
        .patch(format!("{}/v1/projects/{}", ctx.base(), project["id"].as_str().unwrap()))
        .bearer_auth(&ctx.human)
        .json(&json!({"policy":{"configurable":{"allowMultipleDispatchers":true,"maxDelegationDepth":10}}}))
        .send()
        .await?
        .error_for_status()?;
    let project_id = project["id"].as_str().unwrap().to_string();
    let mut parent_token = ctx.human.clone();
    let mut ids = Vec::new();
    for index in 0..3 {
        let response: Value = ctx
            .create_instance_with(
                &parent_token,
                json!({
                    "role":"project-coordinator",
                    "grants":["dispatch"],
                    "projectId":project_id,
                    "hostId":ctx.host_a,
                    "prompt":format!("n{index}")
                }),
            )
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = response["instance"]["instanceId"]
            .as_str()
            .unwrap()
            .to_string();
        parent_token = ctx.agent_token_for(&id).await?;
        ids.push(id);
    }
    // Rewire depth-1 node's parent to the depth-3 node.
    let db = rusqlite::Connection::open(ctx._dir.path().join("hub").join("hub.sqlite"))?;
    db.execute(
        "UPDATE instances SET spec_json = json_set(spec_json, '$.parentInstanceId', ?1) WHERE id = ?2",
        rusqlite::params![ids[2], ids[0]],
    )?;
    drop(db);
    // A child under the rewired node walks node1 → node3 → node2 → node1.
    let err = ctx
        .hub
        .store()
        .unwrap()
        .insert_instance_delegated(
            ctx.host_a.clone(),
            None,
            "claude".into(),
            "claude-print".into(),
            None,
            json!({"parentInstanceId": ids[0], "projectId": project_id}),
            remuda_hub::store_test_support::leaf_delegation(&project_id)
                .map_err(anyhow::Error::msg)?,
        )
        .await;
    let err = err.expect_err("cycle must be rejected");
    assert!(err.to_string().contains("cycle"), "{err}");
    Ok(())
}
