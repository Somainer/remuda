//! M1 batch 5a (co-dispatch): worker roster CRUD, `POST /v1/workers/dispatch`
//! against a fake Node that answers `worker.provision` / `instance.create` /
//! `instance.send`, product-assigned port blocks, retire refusal and
//! force-reclaim, and `GET /v1/hosts/{id}/hostcap`.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// One connected fake Node that answers worker RPCs and records provision calls.
struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    /// Methods this node was asked to call (in order).
    calls: mpsc::UnboundedReceiver<String>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
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

async fn enroll_node(
    hub: &remuda_hub::RunningHub,
    host_id: &str,
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
                    "labels": {"toolchain": "rust"},
                    "maxInstances": max_instances,
                    "resources": resources,
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr": {"version": "0.9.0", "socket": "/tmp/fake-herdr.sock"},
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
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let Ok(frame) = recv_json(&mut node).await else {
                break;
            };
            if let Some(id) = frame.get("id")
                && let Some(method) = frame.get("method").and_then(Value::as_str)
            {
                let _ = tx.send(method.to_string());
                let result = match method {
                    "worker.provision" => {
                        let params = frame.get("params").cloned().unwrap_or(json!({}));
                        let name = params["name"].as_str().unwrap_or("x");
                        let branch = params["branch"].as_str().unwrap_or("wt/x/work");
                        json!({
                            "name": name,
                            "branch": branch,
                            "startPoint": "origin/main",
                            "worktreePath": format!("/tmp/remuda-wt/{name}"),
                            "targetDir": format!("/tmp/remuda-target/{name}"),
                        })
                    }
                    "worker.remove" => json!({
                        "name": frame["params"]["name"].as_str().unwrap_or("x"),
                        "worktreeRemoved": true,
                        "targetRemoved": true,
                        "reclaimedBytes": "4096",
                    }),
                    "instance.create" => {
                        json!({"accepted": true, "instanceId": "ins_fake00000000000000000000000001"})
                    }
                    "instance.send" => json!({"accepted": true}),
                    "instance.close" => json!({"accepted": true}),
                    _ => json!({"ok": true}),
                };
                let response = json!({
                    "jsonrpc": "2.0", "id": id, "result": result,
                });
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
    Ok(FakeNode { task, calls: rx })
}

const BRIEF: &str =
    "Do the tiny task in $WORKTREE.\nReply on one line: DONE <sha> or BLOCKED <reason>.\n";

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
    node: FakeNode,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    async fn spawn() -> Result<Ctx> {
        Self::spawn_with_resources(json!({
            "cpuCount": 8, "cpuPct": 5, "memPct": 20,
            "loadAvg1": 0.4, "diskFreeGb": 120.0,
        }))
        .await
    }

    async fn spawn_with_resources(resources: Value) -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("dispatch-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = enroll_node(&hub, &host, &[&workspace], resources, 8).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            node,
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        self.request_with_token(method, path, body, &self.human)
            .await
    }

    async fn request_with_token(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> (reqwest::StatusCode, Value) {
        let mut builder = self
            .http
            .request(method.parse().unwrap(), format!("{}{}", self.base(), path))
            .bearer_auth(token);
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(json!(null));
        (status, body)
    }

    async fn create_project(&self, port_blocks: &[&str]) -> Value {
        let body = json!({
            "name": "co-dispatch-test",
            "members": [{ "hostId": self.host, "workspaceId": self.workspace, "role": "build" }],
            "hosts": [{
                "hostId": self.host,
                "maxInstances": 8,
                "maxBuilding": 4,
                "diskBudgetGb": 10,
                "portBlocks": port_blocks,
                "requires": ["toolchain=rust"],
                "latencyClass": "remote",
            }],
        });
        let (status, body) = self.request("POST", "/v1/projects", Some(body)).await;
        assert!(status.is_success(), "{status} {body}");
        body
    }

    fn dispatch_body(&self, project_id: &str) -> Value {
        json!({
            "projectId": project_id,
            "brief": BRIEF,
            "briefName": "brief.md",
            "harness": "claude",
        })
    }

    /// Drain recorded Node methods up to now.
    fn drain_calls(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(method) = self.node.calls.try_recv() {
            out.push(method);
        }
        out
    }
}

#[tokio::test]
async fn dispatch_provisions_records_and_launches() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58600-58629"]).await;
    let project_id = project["id"].as_str().unwrap();

    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(project_id)),
        )
        .await;
    assert!(status.is_success(), "dispatch {status}: {body}");
    let worker = &body["worker"];
    assert_eq!(worker["projectId"].as_str(), Some(project_id));
    assert_eq!(worker["harness"], "claude");
    assert!(worker["branch"].as_str().unwrap().starts_with("wt/"));
    assert_eq!(
        worker["worktreePath"].as_str(),
        Some(format!("/tmp/remuda-wt/{}", worker["name"].as_str().unwrap()).as_str())
    );
    assert_eq!(worker["state"]["state"], "working");
    assert_eq!(worker["portBlock"], "58600-58609");
    assert!(worker["briefObjectId"].as_str().is_some());
    assert!(worker["instanceId"].as_str().is_some());
    assert!(
        worker["targetDir"]
            .as_str()
            .unwrap()
            .contains("remuda-target")
    );

    let calls = ctx.drain_calls();
    assert!(calls.contains(&"worker.provision".to_string()), "{calls:?}");
    assert!(calls.contains(&"instance.create".to_string()), "{calls:?}");
    assert!(calls.contains(&"instance.send".to_string()), "{calls:?}");

    // Roster list + get.
    let (status, body) = ctx
        .request("GET", &format!("/v1/workers?project={project_id}"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let name = worker["name"].as_str().unwrap().to_string();
    let (status, body) = ctx
        .request("GET", &format!("/v1/workers/{name}"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name"].as_str(), Some(name.as_str()));
}

#[tokio::test]
async fn duplicate_worker_name_conflicts() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58700-58729"]).await;
    let project_id = project["id"].as_str().unwrap();
    let mut body = ctx.dispatch_body(project_id);
    body["name"] = json!("dup");
    let (status, _) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body.clone()))
        .await;
    assert!(status.is_success());
    ctx.drain_calls();
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test]
async fn retire_refuses_working_then_force_reclaims() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58800-58829"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (_, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(project_id)),
        )
        .await;
    let worker_id = body["worker"]["id"].as_str().unwrap();
    ctx.drain_calls();

    // Working + no force → 409, nothing reclaimed.
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/retire"),
            Some(json!({ "force": false })),
        )
        .await;
    assert_eq!(status, 409);

    // Mark done, then plain retire succeeds.
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/state"),
            Some(json!({ "state": "done", "sha": "deadbeef" })),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/retire"),
            Some(json!({})),
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["worker"]["state"]["state"], "retired");
    assert_eq!(body["node"]["reclaimedBytes"], "4096");
    assert!(ctx.drain_calls().contains(&"worker.remove".to_string()));

    // Retired state is terminal.
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/state"),
            Some(json!({ "state": "working" })),
        )
        .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn force_retire_working_worker() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58830-58859"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (_, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(project_id)),
        )
        .await;
    let worker_id = body["worker"]["id"].as_str().unwrap();
    ctx.drain_calls();
    let (status, body) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/retire"),
            Some(json!({ "force": true })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["worker"]["state"]["state"], "retired");
    let calls = ctx.drain_calls();
    assert!(calls.contains(&"instance.close".to_string()));
    assert!(calls.contains(&"worker.remove".to_string()));
}

#[tokio::test]
async fn port_blocks_allocate_uniquely_and_exhaust() {
    let mut ctx = Ctx::spawn().await.unwrap();
    // One block available in the project range.
    let project = ctx.create_project(&["58900-58909"]).await;
    let project_id = project["id"].as_str().unwrap();
    let mut first = ctx.dispatch_body(project_id);
    first["name"] = json!("p1");
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(first))
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["worker"]["portBlock"], "58900-58909");
    ctx.drain_calls();

    let mut second = ctx.dispatch_body(project_id);
    second["name"] = json!("p2");
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(second))
        .await;
    assert_eq!(status, 409, "exhausted ranges conflict: {body}");
}

#[tokio::test]
async fn hostcap_reports_capacity_and_blocks() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58640-58669"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (_, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(project_id)),
        )
        .await;
    assert_eq!(body["worker"]["portBlock"], "58640-58649");
    ctx.drain_calls();

    let (status, body) = ctx
        .request("GET", &format!("/v1/hosts/{}/hostcap", ctx.host), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["cores"], 8);
    assert_eq!(body["diskFreeGb"], 120.0);
    assert_eq!(body["loadAvg1"], 0.4);
    // The fake node's resource sample carries a Hub-stamped time; hostcap
    // surfaces both the stamp and its age so a caller can tell a live reading
    // from the fossil the freshness window exists to catch.
    assert!(
        body["sampledAt"].as_str().is_some_and(|s| s.contains('T')),
        "sampledAt must be an RFC3339 stamp: {body}"
    );
    assert!(
        body["sampleAgeSec"]
            .as_i64()
            .is_some_and(|age| (0..=300).contains(&age)),
        "sampleAgeSec must reflect a recent Hub-stamped sample: {body}"
    );
    assert_eq!(body["maxInstances"], 8);
    // The fake node does not project a live instance lifecycle, so the
    // Node-confirmed running count stays 0; roster allocations are counted
    // separately in activeWorkers / portBlocksInUse.
    assert_eq!(body["running"], 0);
    assert_eq!(body["freeSlots"], 8);
    assert_eq!(body["activeWorkers"], 1);
    assert_eq!(body["portBlocksInUse"].as_array().unwrap().len(), 1);
    assert_eq!(body["portBlocksInUse"][0]["block"], "58640-58649");
}

#[tokio::test]
async fn dispatch_pin_admits_over_cpu_ceiling_but_auto_dispatch_refuses() {
    let mut ctx = Ctx::spawn_with_resources(json!({
        "cpuCount": 8, "cpuPct": 100, "memPct": 20,
        "loadAvg1": 8.0, "diskFreeGb": 120.0,
    }))
    .await
    .unwrap();
    let project = ctx.create_project(&["58670-58699"]).await;
    let project_id = project["id"].as_str().unwrap();
    ctx.drain_calls();

    // Auto (project-member) placement refuses the genuinely saturated host.
    let (status, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(project_id)),
        )
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], json!("PLACEMENT_UNSATISFIABLE"));

    // The same host named explicitly is a pin: it launches, and the saturated
    // reading rides the response as a warning.
    let mut pinned = ctx.dispatch_body(project_id);
    pinned["hostId"] = json!(ctx.host);
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(pinned))
        .await;
    assert_eq!(status, 200, "{body}");
    let warnings = body["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("CPU at 100%")),
        "{warnings:?}"
    );
}

/// Create a secret-less gateway profile listing synthetic workhorse models.
async fn create_synth_profile(ctx: &Ctx, models: Value) -> (reqwest::StatusCode, Value) {
    let body = json!({
        "name": "synth-relay",
        "kind": "gateway",
        "baseUrl": "http://127.0.0.1:1",
        "authToken": "sk-synth-secret-cccc",
        "models": models,
    });
    ctx.request("POST", "/v1/providers", Some(body)).await
}

const SYNTH_MODELS: &str = "passthrough/synth/seed-evolving";

#[tokio::test]
async fn dispatch_refuses_unknown_model_pin_without_provisioning() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58910-58939"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (status, profile) = create_synth_profile(
        &ctx,
        json!([
            {"id": SYNTH_MODELS, "family": "synth", "role": "workhorse", "priority": 20}
        ]),
    )
    .await;
    assert!(status.is_success(), "{status} {profile}");
    ctx.drain_calls();

    // The 2026-09-17 shape: pin carries a [1m] tag and lacks the
    // `passthrough/` prefix the catalog id has.
    let mut pinned = ctx.dispatch_body(project_id);
    pinned["model"] = json!("synth/seed-evolving[1m]");
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(pinned))
        .await;
    assert_eq!(status, 409, "expected PIN_REFUSED, got {status} {body}");
    assert_eq!(body["code"], json!("PIN_REFUSED"));
    assert_eq!(body["pin"]["model"], json!("synth/seed-evolving[1m]"));
    let suggestions = body["suggestions"].as_array().unwrap();
    assert!(suggestions.len() <= 5);
    assert!(
        suggestions.iter().any(|s| s == SYNTH_MODELS),
        "suggestions must name the listed id: {body}"
    );
    let reasons = body["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r
            .as_str()
            .unwrap_or("")
            .contains("not listed by any provider profile")),
        "{body}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap_or("").contains("never substituted")),
        "{body}"
    );

    // Nothing reached the Node: no worktree provisioned, no instance launched.
    let calls = ctx.drain_calls();
    assert!(
        !calls.contains(&"worker.provision".to_string()),
        "refusal must not provision: {calls:?}"
    );
    assert!(
        !calls.contains(&"instance.create".to_string()),
        "refusal must not launch: {calls:?}"
    );

    // The roster gets no row.
    let (status, workers) = ctx
        .request("GET", &format!("/v1/workers?project={project_id}"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        workers["items"].as_array().unwrap().len(),
        0,
        "no roster row on refusal: {workers}"
    );
}

#[tokio::test]
async fn dispatch_honors_known_model_pin_and_provisions() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58940-58969"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (status, profile) =
        create_synth_profile(&ctx, json!([
            {"id": SYNTH_MODELS, "family": "synth", "role": "workhorse", "priority": 20},
            {"id": "passthrough/synth/fable-x", "family": "synth", "role": "frontier", "priority": 99},
        ]))
        .await;
    assert!(status.is_success(), "{status} {profile}");
    ctx.drain_calls();

    let mut pinned = ctx.dispatch_body(project_id);
    pinned["model"] = json!(SYNTH_MODELS);
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(pinned))
        .await;
    assert!(
        status.is_success(),
        "known pin must dispatch: {status} {body}"
    );
    // The pin wins even though the fable row declares priority 99.
    assert_eq!(body["worker"]["model"], json!(SYNTH_MODELS));
    assert_eq!(body["worker"]["providerProfileId"], profile["id"], "{body}");
    let calls = ctx.drain_calls();
    assert!(calls.contains(&"worker.provision".to_string()), "{calls:?}");
    assert!(calls.contains(&"instance.create".to_string()), "{calls:?}");
    // No workhorse warning: the project declares no workhorse.
    let warnings = body["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("workhorse")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn dispatch_warns_when_honored_pin_differs_from_project_workhorse() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58970-58999"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (status, profile) =
        create_synth_profile(&ctx, json!([
            {"id": SYNTH_MODELS, "family": "synth", "role": "workhorse", "priority": 20},
            {"id": "passthrough/synth/fable-x", "family": "synth", "role": "frontier", "priority": 99},
        ]))
        .await;
    assert!(status.is_success(), "{status} {profile}");

    // The project speaks model roles: workhorse is the seed model. The API
    // has no workhorse setter yet, so seed the stored project directly.
    let patched = ctx
        .hub
        .store()
        .expect("store")
        .patch_project(project_id.to_string(), |project| {
            project.model_roles.workhorse = Some(SYNTH_MODELS.into());
            Ok(())
        })
        .await
        .expect("patch project");
    assert_eq!(
        patched.expect("project").model_roles.workhorse.as_deref(),
        Some(SYNTH_MODELS)
    );
    ctx.drain_calls();

    // Pin the non-workhorse model: admitted (warning, not refusal).
    let mut pinned = ctx.dispatch_body(project_id);
    pinned["model"] = json!("passthrough/synth/fable-x");
    let (status, body) = ctx
        .request("POST", "/v1/workers/dispatch", Some(pinned))
        .await;
    assert!(
        status.is_success(),
        "honored pin dispatches: {status} {body}"
    );
    assert_eq!(body["worker"]["model"], json!("passthrough/synth/fable-x"));
    let warnings = body["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings.iter().any(|w| {
            let text = w.as_str().unwrap_or("");
            text.contains("differs from the project workhorse") && text.contains(SYNTH_MODELS)
        }),
        "expected informational workhorse warning: {warnings:?}"
    );
}

#[tokio::test]
async fn dispatch_rejects_brief_and_unknown_project() {
    let ctx = Ctx::spawn().await.unwrap();
    // Empty brief.
    let body = json!({
        "projectId": "prj_01999999-0000-7000-8000-000000000001",
        "brief": "  ",
    });
    let (status, _) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    assert_eq!(status, 400);
    // Unknown project.
    let body = json!({
        "projectId": "prj_01999999-0000-7000-8000-000000000009",
        "brief": BRIEF,
    });
    let (status, _) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn state_done_requires_sha_and_blocked_requires_reason() {
    let mut ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58670-58699"]).await;
    let project_id = project["id"].as_str().unwrap();
    let (_, body) = ctx
        .request(
            "POST",
            "/v1/workers/dispatch",
            Some(ctx.dispatch_body(project_id)),
        )
        .await;
    let worker_id = body["worker"]["id"].as_str().unwrap();
    ctx.drain_calls();
    let (status, _) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/state"),
            Some(json!({ "state": "done" })),
        )
        .await;
    assert_eq!(status, 400);
    let (status, body) = ctx
        .request(
            "POST",
            &format!("/v1/workers/{worker_id}/state"),
            Some(json!({ "state": "blocked", "reason": "need creds" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"]["reason"], "need creds");
}

#[tokio::test]
async fn dispatch_refuses_computer_use_for_every_harness_without_downgrading() {
    // D-045 Q4: dispatch workers are unattended; the grant is refused for both
    // harnesses (never silently flipped to host approvals) and the refusal
    // arrives before provisioning, so no worktree is created.
    for harness in ["claude", "codex"] {
        let mut ctx = Ctx::spawn().await.unwrap();
        let project = ctx.create_project(&["58940-58969"]).await;
        let project_id = project["id"].as_str().unwrap();
        ctx.drain_calls();

        let mut body = ctx.dispatch_body(project_id);
        body["harness"] = json!(harness);
        body["capabilities"] = json!(["computer-use"]);
        let (status, response) = ctx
            .request("POST", "/v1/workers/dispatch", Some(body))
            .await;
        assert_eq!(status, 400, "harness {harness}: {response}");
        let message = response["error"].as_str().unwrap_or_default();
        assert!(
            message.contains("dispatch") && message.contains("unattended"),
            "harness {harness}: {message}"
        );

        // No provisioning happened for the refused dispatch.
        assert!(
            !ctx.drain_calls()
                .iter()
                .any(|method| method == "worker.provision"),
            "harness {harness}: refused dispatch must not provision"
        );
    }
}

#[tokio::test]
async fn agent_origin_create_with_computer_use_is_refused_before_placement() {
    // D-045 gate 1 at the Hub: an instance-scoped agent credential may not
    // grant computer-use to itself, regardless of host or permission mode.
    let ctx = Ctx::spawn().await.unwrap();
    // Seed a leaf instance and mint its instance token (origin = Agent).
    let project = ctx.create_project(&["58940-58969"]).await;
    let project_id = project["id"].as_str().unwrap().to_string();
    let mut delegation =
        remuda_hub::store_test_support::leaf_delegation(&project_id).expect("leaf delegation");
    // prepare_create requires the Dispatch grant even for a same-host agent.
    delegation.grants = vec!["dispatch".to_owned()];
    let instance = ctx
        .hub
        .store()
        .unwrap()
        .insert_instance_delegated(
            ctx.host.clone(),
            Some(ctx.workspace.clone()),
            "claude".into(),
            "claude-print".into(),
            None,
            json!({ "projectId": project_id }),
            delegation,
        )
        .await
        .expect("seed instance");
    let agent_token =
        remuda_hub::instance_token(ctx.hub.store().unwrap().clone(), instance.instance_id)
            .await
            .expect("agent token");

    let body = json!({
        "hostId": ctx.host,
        "kind": "claude",
        "driver": "claude-print",
        "permissionMode": "manual",
        "capabilities": ["computer-use"],
        "prompt": "drive",
    });
    let (status, response) = ctx
        .request_with_token("POST", "/v1/instances", Some(body), &agent_token)
        .await;
    assert_eq!(status, 400, "{response}");
    let message = response["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("agent-originated") && message.contains("computer-use"),
        "{message}"
    );
}

#[tokio::test]
async fn instance_create_refuses_codex_unattended_spellings_with_computer_use() {
    // D-045 Q4 at the create endpoint: the Hub gate must understand codex's
    // auto-approve spellings (never / no-request), not only claude's
    // bypassPermissions, and refuse before persistence.
    let ctx = Ctx::spawn().await.unwrap();
    for (kind, mode) in [
        ("claude", "bypassPermissions"),
        ("codex", "never"),
        ("codex", "no-request"),
    ] {
        let body = json!({
            "hostId": ctx.host,
            "kind": kind,
            "driver": "shell-pty",
            "permissionMode": mode,
            "capabilities": ["computer-use"],
            "prompt": "drive",
        });
        let (status, response) = ctx.request("POST", "/v1/instances", Some(body)).await;
        assert_eq!(
            status, 400,
            "kind={kind} mode={mode} must be refused: {response}"
        );
        let message = response["error"].as_str().unwrap_or_default();
        assert!(
            message.contains("unattended") && message.contains(kind),
            "must name both the condition and the harness ({kind}/{mode}): {message}"
        );
    }

    // A non-unattended codex spelling passes the Q4 gate and reaches the host
    // preflight (which fails only because the fake node reports no row here).
    let body = json!({
        "hostId": ctx.host,
        "kind": "codex",
        "driver": "shell-pty",
        "permissionMode": "on-request",
        "capabilities": ["computer-use"],
        "prompt": "drive",
    });
    let (status, response) = ctx.request("POST", "/v1/instances", Some(body)).await;
    assert_eq!(
        status, 400,
        "expected the host preflight refusal, not success: {response}"
    );
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not reported"),
        "should reach the host-capability gate: {response}"
    );
}

#[tokio::test]
async fn dispatch_rejects_unknown_capability_value() {
    let ctx = Ctx::spawn().await.unwrap();
    let project = ctx.create_project(&["58970-58999"]).await;
    let project_id = project["id"].as_str().unwrap();
    let mut body = ctx.dispatch_body(project_id);
    body["capabilities"] = json!(["desktop"]);
    let (status, response) = ctx
        .request("POST", "/v1/workers/dispatch", Some(body))
        .await;
    assert_eq!(status, 400, "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("\"desktop\""),
        "{response}"
    );
}
