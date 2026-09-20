//! Coordinator batch 4 (co-task): the task ledger state machine, dependency
//! edges unlocked only by a landed sha (never a worker status bit — §7 #8),
//! ownership-claim conflicts at claim time, `own check` against a synthetic
//! diff crossing a boundary, placement-ledger rows (`reasons[]`/`rejected[]`)
//! round-tripping, the §2.5 mandate chain on split, and scope/grant
//! enforcement for the new routes.
//!
//! Everything runs against the in-process Hub with human device tokens (and
//! one store-inserted leaf worker for the 403 case) — no fake node, no real
//! models.

use anyhow::Result;
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("task-human").await?;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let mut builder = self
            .http
            .request(method.parse()?, format!("{}{}", self.base(), path))
            .bearer_auth(token);
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        Ok(builder.send().await?)
    }

    async fn json_ok(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let response = self.request(method, path, &self.human, body).await?;
        let status = response.status();
        let value: Value = response.json().await?;
        anyhow::ensure!(status.is_success(), "{method} {path} → {status}: {value}");
        Ok(value)
    }

    async fn create_project(&self, name: &str) -> Result<String> {
        let project = self
            .json_ok("POST", "/v1/projects", Some(json!({ "name": name })))
            .await?;
        Ok(project["id"].as_str().unwrap().to_string())
    }

    async fn add_task(&self, project: &str, title: &str, body_extra: Value) -> Result<Value> {
        let mut body = json!({
            "projectId": project,
            "title": title,
            "intent": format!("owner intent for {title}"),
        });
        if let Some(object) = body_extra.as_object() {
            for (key, value) in object {
                body[key] = value.clone();
            }
        }
        self.json_ok("POST", "/v1/tasks", Some(body)).await
    }

    async fn set_state(&self, task: &str, state: &str) -> Result<reqwest::Response> {
        self.request(
            "PATCH",
            &format!("/v1/tasks/{task}"),
            &self.human,
            Some(json!({ "state": state })),
        )
        .await
    }

    async fn board(&self, project: &str) -> Result<Value> {
        self.json_ok("GET", &format!("/v1/board?project={project}"), None)
            .await
    }

    async fn archive(&self, task: &str) -> Result<reqwest::Response> {
        self.request(
            "POST",
            &format!("/v1/tasks/{task}/archive"),
            &self.human,
            None,
        )
        .await
    }
}

/// Map of `taskId → boardColumn` from a board response.
fn column_index(board: &Value) -> serde_json::Map<String, Value> {
    let mut index = serde_json::Map::new();
    for (column, cards) in board["columns"].as_object().unwrap() {
        for card in cards.as_array().unwrap() {
            assert_eq!(card["boardColumn"].as_str(), Some(column.as_str()));
            index.insert(card["id"].as_str().unwrap().to_string(), json!(column));
        }
    }
    index
}

#[tokio::test]
async fn state_machine_accepts_legal_lifecycle_and_rejects_illegal_jumps() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("states").await?;
    let task = ctx.add_task(&project, "lifecycle", json!({})).await?;
    let task_id = task["id"].as_str().unwrap();
    assert_eq!(task["state"], "pending");

    // pending → running skips `placed`; 409 conflict.
    let jumped = ctx.set_state(task_id, "running").await?;
    assert_eq!(jumped.status(), 409);
    let body: Value = jumped.json().await?;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("illegal task transition"),
        "{body}"
    );

    // Legal lifecycle: pending → placed → running → stalled → running → done.
    for state in ["placed", "running", "stalled", "running", "done"] {
        let response = ctx.set_state(task_id, state).await?;
        assert_eq!(response.status(), 200, "transition to {state}");
    }
    // Terminal: done cannot move.
    assert_eq!(ctx.set_state(task_id, "running").await?.status(), 409);

    // failed carries the BLOCKED reason and is terminal too.
    let other = ctx.add_task(&project, "failing", json!({})).await?;
    let other_id = other["id"].as_str().unwrap();
    let response = ctx
        .request(
            "PATCH",
            &format!("/v1/tasks/{other_id}"),
            &ctx.human,
            Some(json!({"state": "failed", "reason": "needs a secret"})),
        )
        .await?;
    assert_eq!(response.status(), 200);
    let failed: Value = ctx
        .json_ok("GET", &format!("/v1/tasks/{other_id}"), None)
        .await?;
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["blockedReason"], "needs a secret");
    assert_eq!(ctx.set_state(other_id, "pending").await?.status(), 409);

    // Unknown state names are 400, missing task is 404.
    let bad = ctx
        .request(
            "PATCH",
            &format!("/v1/tasks/{task_id}"),
            &ctx.human,
            Some(json!({ "state": "nope" })),
        )
        .await?;
    assert_eq!(bad.status(), 400);
    let missing = remuda_protocol::TaskId::new();
    let gone = ctx.set_state(missing.as_id().as_str(), "running").await?;
    assert_eq!(gone.status(), 404);

    // List filters.
    let list: Value = ctx
        .json_ok(
            "GET",
            &format!("/v1/tasks?project={project}&state=failed"),
            None,
        )
        .await?;
    let ids: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [other_id]);
    ctx.hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn dependency_edges_unlock_only_from_a_landed_sha() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("deps").await?;
    let dep = ctx.add_task(&project, "api-first", json!({})).await?;
    let dep_id = dep["id"].as_str().unwrap().to_string();
    let dependent = ctx
        .add_task(
            &project,
            "cli-second",
            json!({ "deps": [{ "taskId": dep_id, "note": "API first" }] }),
        )
        .await?;
    let dependent_id = dependent["id"].as_str().unwrap().to_string();
    assert_eq!(
        dependent["deps"][0]["taskId"].as_str(),
        Some(dep_id.as_str())
    );

    // A dep referencing a task in another project is refused.
    let other_project = ctx.create_project("other").await?;
    let cross = ctx
        .request(
            "POST",
            "/v1/tasks",
            &ctx.human,
            Some(json!({
                "projectId": other_project,
                "title": "cross",
                "intent": "cross-project dep",
                "deps": [{ "taskId": dep_id }],
            })),
        )
        .await?;
    assert_eq!(cross.status(), 400);
    // Worker claims DONE with no sha: the edge stays locked.
    for state in ["placed", "running", "done"] {
        assert_eq!(ctx.set_state(&dep_id, state).await?.status(), 200);
    }
    let view = ctx
        .json_ok("GET", &format!("/v1/tasks/{dependent_id}"), None)
        .await?;
    assert_eq!(
        view["lockedDeps"].as_array().unwrap(),
        &[json!(dep_id)],
        "a worker `done` status bit never unlocks an edge (§7 #8)"
    );

    // Land records the sha: the edge unlocks and the task flips to done.
    let landed = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{dep_id}/land"),
            Some(json!({ "sha": "0123456789abcdef0123456789abcdef01234567" })),
        )
        .await?;
    assert_eq!(landed["state"], "done");
    assert_eq!(
        landed["landedSha"],
        "0123456789abcdef0123456789abcdef01234567",
    );
    let view = ctx
        .json_ok("GET", &format!("/v1/tasks/{dependent_id}"), None)
        .await?;
    assert!(view["lockedDeps"].as_array().unwrap().is_empty());

    // A malformed sha is 400; landing a never-run task is 409.
    let pending = ctx.add_task(&project, "unstarted", json!({})).await?;
    let pending_id = pending["id"].as_str().unwrap();
    let bad_sha = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{pending_id}/land"),
            &ctx.human,
            Some(json!({ "sha": "xyz" })),
        )
        .await?;
    assert_eq!(bad_sha.status(), 400);
    let early = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{pending_id}/land"),
            &ctx.human,
            Some(json!({ "sha": "abcdef7" })),
        )
        .await?;
    assert_eq!(early.status(), 409, "cannot land a pending task");
    Ok(())
}

#[tokio::test]
async fn own_claim_conflicts_are_rejected_and_check_judges_a_synthetic_diff() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("owns").await?;
    let task_a = ctx
        .add_task(
            &project,
            "hub-side",
            json!({ "owns": ["crates/remuda-hub/src/tasks.rs"] }),
        )
        .await?;
    let a_id = task_a["id"].as_str().unwrap().to_string();
    let task_b = ctx.add_task(&project, "hub-subtree", json!({})).await?;
    let b_id = task_b["id"].as_str().unwrap().to_string();

    // B tries to claim a subtree containing A's file → 409 naming the holder.
    let conflict = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{b_id}/own"),
            &ctx.human,
            Some(json!({ "paths": ["crates/remuda-hub/src/"] })),
        )
        .await?;
    assert_eq!(conflict.status(), 409);
    let body: Value = conflict.json().await?;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("crates/remuda-hub/src/tasks.rs"),
        "{body}"
    );

    // An unrelated tree claims cleanly; a second unrelated task can sit beside
    // both, and the ownership map lists exactly the active claims.
    let claim_b = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{b_id}/own"),
            Some(json!({ "paths": ["crates/remuda-protocol/src/task.rs"] })),
        )
        .await?;
    assert_eq!(
        claim_b["owns"],
        json!(["crates/remuda-protocol/src/task.rs"])
    );
    let map: Value = ctx
        .json_ok("GET", &format!("/v1/own?project={project}"), None)
        .await?;
    let entries = map["items"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries.iter().any(|row| row["taskId"] == a_id
            && row["paths"] == json!(["crates/remuda-hub/src/tasks.rs"]))
    );

    // own check on a synthetic diff: one file inside, one crossing the border.
    let diff = "\
diff --git a/crates/remuda-hub/src/tasks.rs b/crates/remuda-hub/src/tasks.rs
index 1111..2222 100644
--- a/crates/remuda-hub/src/tasks.rs
+++ b/crates/remuda-hub/src/tasks.rs
@@ -1 +1 @@
-old
+new
diff --git a/crates/remuda/src/cmd/merge.rs b/crates/remuda/src/cmd/merge.rs
--- a/crates/remuda/src/cmd/merge.rs
+++ b/crates/remuda/src/cmd/merge.rs
@@ -1 +1 @@
-x
+y
";
    let crossed = ctx
        .json_ok(
            "POST",
            "/v1/own/check",
            Some(json!({ "taskId": a_id, "diff": diff })),
        )
        .await?;
    assert_eq!(crossed["within"], false);
    assert_eq!(crossed["checked"], 2);
    assert_eq!(
        crossed["violations"],
        json!(["crates/remuda/src/cmd/merge.rs"])
    );

    // Explicit paths entirely inside the claim are clean.
    let within = ctx
        .json_ok(
            "POST",
            "/v1/own/check",
            Some(json!({
                "taskId": a_id,
                "paths": ["crates/remuda-hub/src/tasks.rs"],
            })),
        )
        .await?;
    assert_eq!(within["within"], true);
    assert!(within["violations"].as_array().unwrap().is_empty());

    // Releasing A's claim lets B take the subtree; releasing again is a no-op.
    let released = ctx
        .json_ok(
            "DELETE",
            &format!("/v1/tasks/{a_id}/own"),
            Some(json!({ "paths": [] })),
        )
        .await?;
    assert!(released["owns"].as_array().unwrap().is_empty());
    let now_ok = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{b_id}/own"),
            Some(json!({ "paths": ["crates/remuda-hub/src/"] })),
        )
        .await?;
    assert!(
        now_ok["owns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "crates/remuda-hub/src/**"),
        "trailing-directory globs normalize to the subtree form"
    );
    Ok(())
}

#[tokio::test]
async fn terminal_tasks_release_claims() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("release").await?;
    let task = ctx.add_task(&project, "short", json!({})).await?;
    let id = task["id"].as_str().unwrap();
    ctx.json_ok(
        "POST",
        &format!("/v1/tasks/{id}/own"),
        Some(json!({ "paths": ["docs/design/x.md"] })),
    )
    .await?;
    for state in ["placed", "running"] {
        assert_eq!(ctx.set_state(id, state).await?.status(), 200);
    }
    ctx.json_ok(
        "POST",
        &format!("/v1/tasks/{id}/land"),
        Some(json!({ "sha": "abcdef0" })),
    )
    .await?;
    let map: Value = ctx
        .json_ok("GET", &format!("/v1/own?project={project}"), None)
        .await?;
    assert!(
        map["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["taskId"] != id),
        "landing releases the task's path claims"
    );
    // …so a new task can claim the same file.
    let next = ctx.add_task(&project, "next", json!({})).await?;
    let next_id = next["id"].as_str().unwrap();
    let claim = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{next_id}/own"),
            Some(json!({ "paths": ["docs/design/x.md"] })),
        )
        .await?;
    assert_eq!(claim["owns"], json!(["docs/design/x.md"]));
    Ok(())
}

#[tokio::test]
async fn placement_ledger_round_trip_and_state_transitions() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("placements").await?;
    let task = ctx.add_task(&project, "placed", json!({})).await?;
    let id = task["id"].as_str().unwrap().to_string();

    // A dispatch row drives pending → placed and pins the slot.
    let dispatch = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            Some(json!({
                "kind": "dispatch",
                "model": "workhorse-1",
                "branch": "wt/w1/placed",
                "reasons": ["member host idle", "matches toolchain=rust"],
                "rejected": [
                    { "candidate": "hst_busy", "reason": "capacity: cpuPct 97" }
                ],
            })),
        )
        .await?;
    assert_eq!(dispatch["task"]["state"], "placed");
    assert_eq!(dispatch["placement"]["reasons"][0], "member host idle");
    assert_eq!(
        dispatch["placement"]["rejected"][0]["reason"],
        "capacity: cpuPct 97"
    );
    assert_eq!(dispatch["task"]["placement"]["model"], "workhorse-1");

    // switch-model is illegal until running (design §4.6).
    let early_switch = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            &ctx.human,
            Some(json!({ "kind": "switch-model", "model": "sibling-2" })),
        )
        .await?;
    assert_eq!(early_switch.status(), 409);

    assert_eq!(ctx.set_state(&id, "running").await?.status(), 200);
    let switched = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            Some(json!({
                "kind": "switch-model",
                "model": "sibling-2",
                "reasons": ["family-level window observed at T"],
            })),
        )
        .await?;
    assert_eq!(switched["task"]["state"], "running", "switch keeps state");
    assert_eq!(switched["task"]["placement"]["model"], "sibling-2");
    assert_eq!(
        switched["task"]["placement"]["branch"], "wt/w1/placed",
        "the slot's other fields survive a model switch"
    );

    // Park, then unplace back to pending.
    let parked = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            Some(json!({
                "kind": "park",
                "reasons": ["account-level window 100% until resetsAt"],
            })),
        )
        .await?;
    assert_eq!(parked["task"]["state"], "parked");
    let unplaced = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            Some(json!({ "kind": "unplace", "reasons": ["worker replaced"] })),
        )
        .await?;
    assert_eq!(unplaced["task"]["state"], "pending");
    assert!(unplaced["task"]["placement"].is_null());

    // Bad kind and illegal dispatch-from-pending chain are refused.
    let bad_kind = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            &ctx.human,
            Some(json!({ "kind": "teleport" })),
        )
        .await?;
    assert_eq!(bad_kind.status(), 400);
    let park_pending = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{id}/placements"),
            &ctx.human,
            Some(json!({ "kind": "park" })),
        )
        .await?;
    assert_eq!(park_pending.status(), 409, "pending cannot park directly");

    // Ledger round-trip: oldest-first, all reasons/rejected survive restart.
    let list: Value = ctx
        .json_ok("GET", &format!("/v1/tasks/{id}/placements"), None)
        .await?;
    let kinds: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["dispatch", "switch-model", "park", "unplace"]);

    let data = ctx._dir.path().join("hub");
    ctx.hub.shutdown().await;
    let hub = spawn(HubConfig::for_test(data)).await?;
    let http = reqwest::Client::new();
    let restarted: Value = http
        .get(format!("http://{}/v1/tasks/{id}/placements", hub.addr))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(restarted["items"].as_array().unwrap().len(), 4);
    assert_eq!(
        restarted["items"][0]["rejected"][0]["candidate"], "hst_busy",
        "rejected[] survives restart — it is the audit/card record"
    );
    hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn split_inherits_the_mandate_chain_and_respects_depth_policy() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("split").await?;
    let root = ctx
        .add_task(&project, "root", json!({ "class": "implement" }))
        .await?;
    let root_id = root["id"].as_str().unwrap().to_string();
    assert_eq!(root["mandate"]["chain"].as_array().unwrap().len(), 1);
    assert_eq!(
        root["mandate"]["chain"][0]["intent"],
        "owner intent for root"
    );

    let child = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{root_id}/split"),
            Some(json!({
                "title": "child",
                "intent": "split the ledger slice",
                "owns": ["crates/remuda-hub/src/tasks.rs"],
            })),
        )
        .await?;
    let child_id = child["id"].as_str().unwrap().to_string();
    assert_eq!(child["parentTaskId"], root_id);
    assert_eq!(child["projectId"], root["projectId"]);
    let chain = child["mandate"]["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0]["intent"], "owner intent for root", "§2.5");
    assert_eq!(chain[0]["taskId"], root_id);
    assert_eq!(chain[1]["intent"], "split the ledger slice");
    assert_eq!(chain[1]["taskId"], child_id);

    // Cap the project at depth 1: a grandchild split is refused with 409.
    ctx.json_ok(
        "PATCH",
        &format!("/v1/projects/{project}"),
        Some(json!({ "policy": { "configurable": { "maxDelegationDepth": 1 } } })),
    )
    .await?;
    let too_deep = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{child_id}/split"),
            &ctx.human,
            Some(json!({ "title": "grand", "intent": "too deep" })),
        )
        .await?;
    assert_eq!(too_deep.status(), 409);
    assert!(
        too_deep.json::<Value>().await?["error"]
            .as_str()
            .unwrap_or("")
            .contains("depth"),
        "depth-limit message"
    );

    // Raise it again and the same split succeeds; the grandchild still sees
    // the owner's words at depth 2.
    ctx.json_ok(
        "PATCH",
        &format!("/v1/projects/{project}"),
        Some(json!({ "policy": { "configurable": { "maxDelegationDepth": 3 } } })),
    )
    .await?;
    let grand = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{child_id}/split"),
            Some(json!({ "title": "grand", "intent": "state machine" })),
        )
        .await?;
    assert_eq!(
        grand["mandate"]["chain"][0]["intent"],
        "owner intent for root"
    );
    assert_eq!(grand["mandate"]["chain"].as_array().unwrap().len(), 3);
    Ok(())
}

#[tokio::test]
async fn scoped_agents_are_enforced_on_task_routes() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project_a = ctx.create_project("alpha").await?;
    let project_b = ctx.create_project("beta").await?;

    // Insert a host, then a leaf-worker instance scoped to project A, the same
    // way a delegated create would have stored it (but without a fake node).
    let host = remuda_protocol::HostId::new();
    ctx.hub.test_insert_host(host.as_id().as_str()).await?;
    let instance = ctx
        .hub
        .store()
        .unwrap()
        .insert_instance_delegated(
            host.as_id().to_string(),
            None,
            "claude".into(),
            "claude-print".into(),
            None,
            json!({ "projectId": project_a }),
            remuda_hub::store_test_support::leaf_delegation(&project_a)
                .map_err(anyhow::Error::msg)?,
        )
        .await?;
    let worker_token =
        remuda_hub::instance_token(ctx.hub.store().unwrap().clone(), instance.instance_id)
            .await
            .map_err(anyhow::Error::from)?;

    // Leaf worker: no dispatch grant → task writes are 403.
    let denied = ctx
        .request(
            "POST",
            "/v1/tasks",
            &worker_token,
            Some(json!({
                "projectId": project_a,
                "title": "sneaky",
                "intent": "workers cannot log tasks",
            })),
        )
        .await?;
    assert_eq!(denied.status(), 403);

    // Seed a task in each project.
    let task_a = ctx.add_task(&project_a, "a-task", json!({})).await?;
    let a_id = task_a["id"].as_str().unwrap().to_string();
    let task_b = ctx.add_task(&project_b, "b-task", json!({})).await?;
    let b_id = task_b["id"].as_str().unwrap().to_string();

    // The worker sees only project A's slice, cannot read project B's task,
    // and cannot claim paths or move state.
    let list: Value = ctx
        .request("GET", "/v1/tasks", &worker_token, None)
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
    assert!(ids.contains(&a_id.as_str()));
    assert!(!ids.contains(&b_id.as_str()));
    let denied = ctx
        .request("GET", &format!("/v1/tasks/{b_id}"), &worker_token, None)
        .await?;
    assert_eq!(denied.status(), 403);
    let denied = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{a_id}/own"),
            &worker_token,
            Some(json!({ "paths": ["crates/x.rs"] })),
        )
        .await?;
    assert_eq!(denied.status(), 403);
    let denied = ctx
        .request(
            "PATCH",
            &format!("/v1/tasks/{a_id}"),
            &worker_token,
            Some(json!({ "state": "placed" })),
        )
        .await?;
    assert_eq!(denied.status(), 403);

    // own/check is a read-shaped gate primitive: the worker may run it against
    // a task in its own project (the merge gate is the intended caller shape).
    let check = ctx
        .request(
            "POST",
            "/v1/own/check",
            &worker_token,
            Some(json!({ "taskId": a_id, "paths": ["crates/x.rs"] })),
        )
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(check["within"], false);
    // …but not across the scope boundary.
    let denied = ctx
        .request(
            "POST",
            "/v1/own/check",
            &worker_token,
            Some(json!({ "taskId": b_id, "paths": ["crates/y.rs"] })),
        )
        .await?;
    assert_eq!(denied.status(), 403);

    // Land requires the land grant, which a leaf lacks.
    let denied = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{a_id}/land"),
            &worker_token,
            Some(json!({ "sha": "abcdef0" })),
        )
        .await?;
    assert_eq!(denied.status(), 403);

    // The board is a read-shaped projection: the worker may read its own
    // project slice, but never another project's cards.
    let board: Value = ctx
        .request("GET", "/v1/board", &worker_token, None)
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ids: Vec<&str> = board["columns"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|cards| cards.as_array().unwrap())
        .map(|card| card["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&a_id.as_str()));
    assert!(!ids.contains(&b_id.as_str()));

    // Archiving moves state-bearing ledger data, so it needs the dispatch
    // grant the leaf worker does not hold (grant-verb gating, D-050).
    let denied = ctx
        .request(
            "POST",
            &format!("/v1/tasks/{a_id}/archive"),
            &worker_token,
            None,
        )
        .await?;
    assert_eq!(denied.status(), 403);
    Ok(())
}

#[tokio::test]
async fn board_projects_states_placement_failures_and_the_archive_flag() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.create_project("board").await?;

    // One card per work-column state.
    let pending = ctx.add_task(&project, "pending", json!({})).await?;
    let pending_id = pending["id"].as_str().unwrap().to_string();
    let deferred = ctx.add_task(&project, "deferred", json!({})).await?;
    let deferred_id = deferred["id"].as_str().unwrap().to_string();
    assert_eq!(ctx.set_state(&deferred_id, "deferred").await?.status(), 200);
    let parked = ctx.add_task(&project, "parked", json!({})).await?;
    let parked_id = parked["id"].as_str().unwrap().to_string();
    for state in ["placed", "running", "parked"] {
        assert_eq!(ctx.set_state(&parked_id, state).await?.status(), 200);
    }
    let placed = ctx.add_task(&project, "placed", json!({})).await?;
    let placed_id = placed["id"].as_str().unwrap().to_string();
    assert_eq!(ctx.set_state(&placed_id, "placed").await?.status(), 200);

    // In-progress states: running and stalled.
    let running = ctx.add_task(&project, "running", json!({})).await?;
    let running_id = running["id"].as_str().unwrap().to_string();
    for state in ["placed", "running"] {
        assert_eq!(ctx.set_state(&running_id, state).await?.status(), 200);
    }
    let stalled = ctx.add_task(&project, "stalled", json!({})).await?;
    let stalled_id = stalled["id"].as_str().unwrap().to_string();
    for state in ["placed", "running", "stalled"] {
        assert_eq!(ctx.set_state(&stalled_id, state).await?.status(), 200);
    }

    // Failed with no placement (pre-dispatch failure) → to-do, carrying the
    // machine-readable blocked reason for the badge.
    let failed_todo = ctx.add_task(&project, "failed-early", json!({})).await?;
    let failed_todo_id = failed_todo["id"].as_str().unwrap().to_string();
    let response = ctx
        .request(
            "PATCH",
            &format!("/v1/tasks/{failed_todo_id}"),
            &ctx.human,
            Some(json!({ "state": "failed", "reason": "supply exhausted" })),
        )
        .await?;
    assert_eq!(response.status(), 200);

    // Failed after dispatch: the dispatch placement row pins a slot, then the
    // task fails mid-flight → in-progress, not a fifth column and not done.
    let failed_mid = ctx.add_task(&project, "failed-mid", json!({})).await?;
    let failed_mid_id = failed_mid["id"].as_str().unwrap().to_string();
    ctx.json_ok(
        "POST",
        &format!("/v1/tasks/{failed_mid_id}/placements"),
        Some(json!({
            "kind": "dispatch",
            "model": "workhorse-1",
            "branch": "wt/w1/failed-mid",
        })),
    )
    .await?;
    let response = ctx
        .request(
            "PATCH",
            &format!("/v1/tasks/{failed_mid_id}"),
            &ctx.human,
            Some(json!({ "state": "failed", "reason": "worker exited 42" })),
        )
        .await?;
    assert_eq!(response.status(), 200);

    let board = ctx.board(&project).await?;
    assert_eq!(board["project"].as_str(), Some(project.as_str()));
    let columns = &board["columns"];
    let todo_ids: Vec<&str> = columns["todo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["id"].as_str().unwrap())
        .collect();
    assert!(todo_ids.contains(&pending_id.as_str()));
    assert!(todo_ids.contains(&deferred_id.as_str()));
    assert!(todo_ids.contains(&parked_id.as_str()));
    assert!(todo_ids.contains(&placed_id.as_str()));
    assert!(todo_ids.contains(&failed_todo_id.as_str()));
    let progress_ids: Vec<&str> = columns["in-progress"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["id"].as_str().unwrap())
        .collect();
    assert!(progress_ids.contains(&running_id.as_str()));
    assert!(progress_ids.contains(&stalled_id.as_str()));
    assert!(progress_ids.contains(&failed_mid_id.as_str()));
    let failed_mid_card = columns["in-progress"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["id"] == failed_mid_id.as_str())
        .unwrap();
    assert_eq!(failed_mid_card["state"], "failed", "the badge is the state");
    assert_eq!(failed_mid_card["blockedReason"], "worker exited 42");
    assert!(
        failed_mid_card["placement"].is_object(),
        "failed-mid keeps its placement reference"
    );
    assert!(columns["done"].as_array().unwrap().is_empty());
    assert!(columns["archived"].as_array().unwrap().is_empty());

    // Display-only per-project keys: oldest-first SE-01… numbering, and they
    // are not a stored field (absent from the task document itself).
    assert_eq!(columns["todo"][0]["displayKey"].as_str(), Some("SE-01"));
    let raw_pending: Value = ctx
        .json_ok("GET", &format!("/v1/tasks/{pending_id}"), None)
        .await?;
    assert!(raw_pending.get("displayKey").is_none());
    // Pre-archive rows carry no archivedAt key at all (byte-identical).
    assert!(raw_pending.get("archivedAt").is_none());

    // Column moves reuse set_task_state: the pending card reaches in-progress
    // through the legal multi-hop pending → placed → running, and the board
    // follows without any board-specific move verb.
    assert_eq!(ctx.set_state(&pending_id, "placed").await?.status(), 200);
    assert_eq!(ctx.set_state(&pending_id, "running").await?.status(), 200);
    let board = ctx.board(&project).await?;
    let index = column_index(&board);
    assert_eq!(index[pending_id.as_str()], json!("in-progress"));

    // Only `done` projects to the done column.
    assert_eq!(ctx.set_state(&running_id, "done").await?.status(), 200);
    let board = ctx.board(&project).await?;
    let index = column_index(&board);
    assert_eq!(index[running_id.as_str()], json!("done"));

    // Archiving is orthogonal: stamp the flag on the running stalled card,
    // state stays `stalled`, the card moves only to the archive column.
    let archived = ctx.archive(&stalled_id).await?;
    assert_eq!(archived.status(), 200);
    let archived: Value = archived.json().await?;
    assert_eq!(archived["state"], "stalled");
    assert!(archived["archivedAt"].as_str().is_some());
    let board = ctx.board(&project).await?;
    let index = column_index(&board);
    assert_eq!(index[stalled_id.as_str()], json!("archived"));
    assert!(
        board["columns"]["in-progress"]
            .as_array()
            .unwrap()
            .iter()
            .all(|card| card["id"] != stalled_id.as_str()),
        "archived card leaves its state-derived column"
    );
    // Re-archiving is rejected; the task still reads as a task (404 stays
    // reserved for unknown ids).
    assert_eq!(ctx.archive(&stalled_id).await?.status(), 409);

    // The done card never unlocks dependents by itself — even though it sits
    // in the done column, the dependent's edge stays locked until a land.
    let dep = ctx.add_task(&project, "dep", json!({})).await?;
    let dep_id = dep["id"].as_str().unwrap().to_string();
    for state in ["placed", "running", "done"] {
        assert_eq!(ctx.set_state(&dep_id, state).await?.status(), 200);
    }
    let dependent = ctx
        .add_task(
            &project,
            "dependent",
            json!({ "deps": [{ "taskId": dep_id }] }),
        )
        .await?;
    let dependent_id = dependent["id"].as_str().unwrap().to_string();
    let view = ctx
        .json_ok("GET", &format!("/v1/tasks/{dependent_id}"), None)
        .await?;
    assert_eq!(view["lockedDeps"].as_array().unwrap(), &[json!(dep_id)]);
    let landed = ctx
        .json_ok(
            "POST",
            &format!("/v1/tasks/{dep_id}/land"),
            Some(json!({ "sha": "abcdef7" })),
        )
        .await?;
    assert_eq!(landed["state"], "done");
    let view = ctx
        .json_ok("GET", &format!("/v1/tasks/{dependent_id}"), None)
        .await?;
    assert!(view["lockedDeps"].as_array().unwrap().is_empty());
    Ok(())
}
