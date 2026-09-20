//! Task-model t-pool: Hub lease/return routes, the lease table, and the
//! lease-aware reclaim guards on `retire_worker` and `delete_instance`.
//!
//! The Node is a fake WebSocket peer (same shape as `tests/worktree.rs`):
//! actual git/pool behavior is covered in remuda-node unit tests; here we pin
//! the wire whitelist (only `{hostId, workspaceId, name, base, taskId}` is
//! forwarded), refcount rows and the refusal/return routing.

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{
    EntityMeta, HostId, ProjectId, Timestamp, U64, WorkerRoster, WorkerRosterId, WorkerState,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(8);

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> Result<(u16, String)> {
    let client = reqwest::Client::new();
    let mut builder = client
        .request(method.parse()?, format!("http://{addr}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        builder = builder
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    let response = builder.send().await?;
    let status = response.status().as_u16();
    let text = response.text().await?;
    Ok((status, text))
}

/// One captured Hub→Node call.
#[derive(Clone, Debug)]
struct Call {
    method: String,
    params: Value,
}

struct FakeNode {
    task: tokio::task::JoinHandle<()>,
    calls: mpsc::UnboundedReceiver<Call>,
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
    match tokio::time::timeout(TIMEOUT, ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => Ok(serde_json::from_str(&text)?),
        other => Err(anyhow!("unexpected ws frame {other:?}")),
    }
}

async fn enroll_fake_node(hub: &remuda_hub::RunningHub, host_id: &str) -> Result<FakeNode> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut req = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(req).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-tpool-test",
                "label": format!("pool-{host_id}"),
                "host": { "hostname": format!("{host_id}.local"), "maxInstances": 8 }
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
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            let params = frame.get("params").cloned().unwrap_or(json!({}));
            // The whitelist is the security boundary: record every call so
            // tests can prove path/repo never cross.
            let _ = tx.send(Call {
                method: method.to_string(),
                params: params.clone(),
            });
            let result = match method {
                "worktree.lease" => lease_answer(&params),
                "worktree.return" => return_answer(&params),
                "worker.remove" => json!({
                    "name": params["name"].as_str().unwrap_or("x"),
                    "worktreeRemoved": true,
                    "targetRemoved": true,
                    "reclaimedBytes": "2048",
                }),
                "instance.close" | "instance.purge" => json!({ "accepted": true }),
                _ => json!({ "ok": true }),
            };
            let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if node
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(FakeNode { task, calls: rx })
}

/// Canned Node lease answers: root/reuse, pool, and an explicit full pool.
fn lease_answer(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("slot");
    let task_id = params["taskId"].as_str().unwrap_or("tsk_x");
    let branch = format!("wt/{name}/{}", &task_id[..task_id.len().min(12)]);
    match name {
        "full" => json!({
            "deferred": true,
            "code": "SUPPLY_DEFERRED",
            "reason": "worktree pool 'full' is full (4 slots) and no clean parked slot is available",
            "poolSize": 4,
        }),
        "." => json!({
            "name": ".",
            "path": "/tmp/repo",
            "branch": "main",
            "mode": "reuse",
            "state": "leased",
            "refcount": 1,
            "warm": true,
            "queued": false,
            "dirKey": ".",
        }),
        "standalone" => json!({
            "name": "standalone",
            "path": "/tmp/remuda-wt/standalone",
            "branch": "wt/standalone/work",
            "mode": "reuse",
            "state": "leased",
            "refcount": 1,
            "warm": true,
            "queued": false,
            "dirKey": "standalone",
        }),
        _ => json!({
            "name": name,
            "path": format!("/tmp/remuda-wt/{name}"),
            "branch": branch,
            "baseOid": "0123456789abcdef0123456789abcdef01234567",
            "mode": "pool",
            "state": "leased",
            "refcount": 1,
            "warm": false,
            "queued": false,
            "dirKey": name,
        }),
    }
}

fn return_answer(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("slot");
    if name == "." {
        json!({
            "name": ".", "mode": "reuse", "state": "free",
            "refcount": 0, "parked": false, "dirKey": ".",
        })
    } else {
        json!({
            "name": name, "mode": "pool", "state": "parked",
            "refcount": 0, "parked": true, "dirKey": name,
        })
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    token: String,
    host_id: String,
    workspace_id: String,
    node: FakeNode,
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let token = hub.mint_device_token("pool-human").await?;
        let host_id = HostId::new().as_id().to_string();
        let workspace_id = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = enroll_fake_node(&hub, &host_id).await?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            token,
            host_id,
            workspace_id,
            node,
        })
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let (status, text) = http(
            self.hub.addr,
            "POST",
            path,
            &self.token,
            Some(&body.to_string()),
        )
        .await
        .expect("http");
        (
            status,
            serde_json::from_str(&text).unwrap_or(json!({ "raw": text })),
        )
    }

    async fn delete(&self, path: &str) -> (u16, Value) {
        let (status, text) = http(self.hub.addr, "DELETE", path, &self.token, None)
            .await
            .expect("http");
        (
            status,
            serde_json::from_str(&text).unwrap_or(json!({ "raw": text })),
        )
    }

    fn drain_calls(&mut self) -> Vec<Call> {
        let mut out = Vec::new();
        while let Ok(call) = self.node.calls.try_recv() {
            out.push(call);
        }
        out
    }

    fn store(&self) -> &remuda_hub::store_test_support::Store {
        self.hub.store().expect("store")
    }

    /// Seed a done, retireable worker roster row bound to `task`.
    async fn seed_worker(&self, name: &str, task: Option<&str>) -> remuda_protocol::WorkerRoster {
        let now = Timestamp::try_from("2026-01-01T00:00:00.000Z".to_string()).unwrap();
        let worker = WorkerRoster {
            meta: EntityMeta {
                id: WorkerRosterId::new(),
                revision: U64(1),
                created_at: now.clone(),
                updated_at: now,
            },
            project_id: ProjectId::new(),
            name: name.to_string(),
            instance_id: None,
            host_id: self.host_id.parse().unwrap(),
            workspace_id: self.workspace_id.parse().unwrap(),
            harness: "claude".into(),
            driver: Some("shell-pty".into()),
            model: None,
            model_effective: None,
            provider_profile_id: None,
            branch: format!("wt/{name}/seed"),
            worktree_path: format!("/tmp/remuda-wt/{name}"),
            port_block: None,
            target_dir: None,
            brief_object_id: None,
            task_id: task.map(|id| id.parse().unwrap()),
            state: WorkerState::Done {
                sha: "0123456789abcdef".into(),
            },
            watch: None,
            last_nudge_at: None,
            resumed_from: None,
            replace_count: None,
            supply_decision: None,
            reclaimed_bytes: None,
        };
        self.store()
            .insert_worker(worker.clone(), "pool-human".into())
            .await
            .expect("insert worker")
    }
}

// ── lease/return routes ────────────────────────────────────────────────────

#[tokio::test]
async fn lease_forwards_only_the_safe_params_and_records_a_pool_row() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let task = remuda_protocol::TaskId::new();
    // The client may send junk (path/repo); the Hub must drop it.
    let body = json!({
        "hostId": ctx.host_id,
        "workspaceId": ctx.workspace_id,
        "taskId": task.as_id().as_str(),
        "base": "main",
        "path": "/etc/escape",
        "repo": "/tmp/repo",
    });
    let (status, value) = ctx.post("/v1/worktrees/slot-a/lease", body).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["mode"], "pool");
    assert_eq!(value["state"], "leased");
    assert_eq!(value["refcount"], 1);
    assert_eq!(value["dirKey"], "slot-a");
    assert!(value["leaseId"].as_str().is_some());

    let calls = ctx.drain_calls();
    let lease_call = calls
        .iter()
        .find(|call| call.method == "worktree.lease")
        .context("worktree.lease forwarded")?;
    // Acceptance #7: path/repo never cross the wire.
    assert!(lease_call.params.get("path").is_none(), "{lease_call:?}");
    assert!(lease_call.params.get("repo").is_none(), "{lease_call:?}");
    for key in ["hostId", "workspaceId", "name", "base", "taskId"] {
        assert!(lease_call.params.get(key).is_some(), "missing {key}");
    }

    let row = ctx
        .store()
        .get_worktree_lease(
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "slot-a".into(),
        )
        .await?
        .expect("lease row");
    assert_eq!(row.mode, "pool");
    assert_eq!(row.refcount, 1);
    assert_eq!(row.state, "leased");
    assert_eq!(row.worktree_name.as_deref(), Some("slot-a"));
    assert_eq!(row.task_ids, vec![task.as_id().to_string()]);

    // Return parks the slot warm and zeroes the refcount.
    let (status, value) = ctx
        .post(
            "/v1/worktrees/slot-a/return",
            json!({"hostId": ctx.host_id, "workspaceId": ctx.workspace_id, "taskId": task.as_id().as_str()}),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["state"], "parked");
    assert_eq!(value["parked"], true);
    assert_eq!(value["refcount"], 0);

    let row = ctx
        .store()
        .get_worktree_lease(
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "slot-a".into(),
        )
        .await?
        .expect("parked row retained");
    assert_eq!(row.state, "parked");
    assert_eq!(row.refcount, 0);
    assert!(row.holder_instance_id.is_none());
    Ok(())
}

#[tokio::test]
async fn full_pool_defers_with_429_and_records_no_lease() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let task = remuda_protocol::TaskId::new();
    let (status, value) = ctx
        .post(
            "/v1/worktrees/full/lease",
            json!({
                "hostId": ctx.host_id,
                "workspaceId": ctx.workspace_id,
                "taskId": task.as_id().as_str(),
            }),
        )
        .await;
    // Acceptance #1: explicit refusal, never a silent reroute.
    assert_eq!(status, 429, "{value}");
    assert_eq!(value["code"], "SUPPLY_DEFERRED");
    let row = ctx
        .store()
        .get_worktree_lease(ctx.host_id.clone(), ctx.workspace_id.clone(), "full".into())
        .await?;
    assert!(row.is_none(), "a deferred lease writes no row");
    let _ = ctx.drain_calls();
    Ok(())
}

#[tokio::test]
async fn reuse_to_root_lands_a_null_name_dot_key_row() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let task = remuda_protocol::TaskId::new();
    let (status, value) = ctx
        .post(
            "/v1/worktrees/-/lease",
            json!({
                "hostId": ctx.host_id,
                "workspaceId": ctx.workspace_id,
                "taskId": task.as_id().as_str(),
            }),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["mode"], "reuse", "root lease payload: {value}");
    assert_eq!(value["dirKey"], ".");

    // Acceptance #6: reuse-to-root row, NULL worktree name, '.' dir key.
    let row = ctx
        .store()
        .get_worktree_lease(ctx.host_id.clone(), ctx.workspace_id.clone(), ".".into())
        .await?
        .expect("root lease row");
    assert_eq!(row.mode, "reuse");
    assert_eq!(row.dir_key, ".");
    assert!(row.worktree_name.is_none());
    assert_eq!(row.refcount, 1);

    let (status, value) = ctx
        .post(
            "/v1/worktrees/-/return",
            json!({"hostId": ctx.host_id, "workspaceId": ctx.workspace_id, "taskId": task.as_id().as_str()}),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["state"], "free");
    // A reuse row at zero is deleted: the Hub keeps no claim on the root.
    let row = ctx
        .store()
        .get_worktree_lease(ctx.host_id.clone(), ctx.workspace_id.clone(), ".".into())
        .await?;
    assert!(row.is_none());
    let _ = ctx.drain_calls();
    Ok(())
}

#[tokio::test]
async fn second_task_shares_refcount_and_queues() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let t1 = remuda_protocol::TaskId::new();
    let t2 = remuda_protocol::TaskId::new();
    let lease_body = |task: &str| {
        json!({
            "hostId": ctx.host_id.clone(),
            "workspaceId": ctx.workspace_id.clone(),
            "taskId": task,
        })
    };
    let (status, first) = ctx
        .post(
            "/v1/worktrees/standalone/lease",
            lease_body(t1.as_id().as_str()),
        )
        .await;
    assert_eq!(status, 200, "{first}");
    let (status, second) = ctx
        .post(
            "/v1/worktrees/standalone/lease",
            lease_body(t2.as_id().as_str()),
        )
        .await;
    assert_eq!(status, 200, "{second}");
    // The second sharer is queued/blocked, never run concurrently.
    assert_eq!(second["refcount"], 2);
    assert_eq!(second["queued"], true);
    assert!(second.get("blocked").is_some());

    let row = ctx
        .store()
        .get_worktree_lease(
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "standalone".into(),
        )
        .await?
        .expect("shared row");
    assert_eq!(row.refcount, 2);
    assert_eq!(row.task_ids.len(), 2);
    let _ = ctx.drain_calls();
    Ok(())
}

// ── retire / delete reclaim guards ─────────────────────────────────────────

#[tokio::test]
async fn retire_of_a_shared_slot_is_refused_and_never_reaches_remove() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let own = remuda_protocol::TaskId::new();
    let other = remuda_protocol::TaskId::new();
    let worker = ctx.seed_worker("shared", Some(own.as_id().as_str())).await;

    // Two tasks hold the slot (one is the retiring worker's own).
    ctx.store()
        .record_worktree_lease(
            "pool".into(),
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "shared".into(),
            Some("shared".into()),
            Some("wt/shared/work".into()),
            None,
            own.as_id().to_string(),
            None,
        )
        .await?;
    ctx.store()
        .record_worktree_lease(
            "pool".into(),
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "shared".into(),
            Some("shared".into()),
            None,
            None,
            other.as_id().to_string(),
            None,
        )
        .await?;

    let (status, value) = ctx
        .post(
            &format!("/v1/workers/{}/retire", worker.meta.id.as_id()),
            json!({ "force": true }),
        )
        .await;
    // Acceptance #2: refcount > 1 refuses force-remove.
    assert_eq!(status, 409, "{value}");
    let message = value["error"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("leased by 2 task(s)"), "{message}");

    let calls = ctx.drain_calls();
    assert!(
        !calls.iter().any(|call| call.method == "worker.remove"),
        "worker.remove must not fire for a shared slot: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.method == "worktree.return"),
        "a shared slot is not returned on the retiree's behalf: {calls:?}"
    );

    // The worker is not retired either.
    let row = ctx
        .store()
        .get_worker(worker.meta.id.as_id().to_string())
        .await?
        .expect("row");
    assert!(!matches!(row.state, WorkerState::Retired));
    Ok(())
}

#[tokio::test]
async fn retire_of_a_sole_holder_returns_and_parks_instead_of_removing() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let own = remuda_protocol::TaskId::new();
    let worker = ctx.seed_worker("solo", Some(own.as_id().as_str())).await;
    ctx.store()
        .record_worktree_lease(
            "pool".into(),
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "solo".into(),
            Some("solo".into()),
            Some("wt/solo/work".into()),
            None,
            own.as_id().to_string(),
            None,
        )
        .await?;

    let (status, value) = ctx
        .post(
            &format!("/v1/workers/{}/retire", worker.meta.id.as_id()),
            json!({ "force": true }),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["worker"]["state"]["state"], "retired");
    assert_eq!(value["node"]["parked"], true);
    assert_eq!(value["node"]["worktreeRemoved"], false);

    let calls = ctx.drain_calls();
    let returned = calls
        .iter()
        .find(|call| call.method == "worktree.return")
        .context("sole-holder retire returns the lease")?;
    assert_eq!(returned.params["name"], "solo");
    assert_eq!(returned.params["taskId"], own.as_id().as_str());
    assert!(returned.params.get("path").is_none());
    assert!(
        !calls.iter().any(|call| call.method == "worker.remove"),
        "the legacy force-remove must not fire: {calls:?}"
    );

    let row = ctx
        .store()
        .get_worktree_lease(ctx.host_id.clone(), ctx.workspace_id.clone(), "solo".into())
        .await?
        .expect("parked row kept");
    assert_eq!(row.state, "parked");
    assert_eq!(row.refcount, 0);
    Ok(())
}

#[tokio::test]
async fn retire_resolves_a_pool_suffixed_slot_back_to_the_worker_name() -> Result<()> {
    // A dispatch leases a pool named after the worker; the Node allocates the
    // slot `<worker>-s1`, so the lease dir key is suffixed while the roster
    // name is bare. The retire guard must still find it.
    let mut ctx = Ctx::spawn().await?;
    let own = remuda_protocol::TaskId::new();
    let other = remuda_protocol::TaskId::new();
    let worker = ctx
        .seed_worker("suffixed", Some(own.as_id().as_str()))
        .await;

    // Shared lease whose key is the suffixed slot.
    ctx.store()
        .record_worktree_lease(
            "pool".into(),
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "suffixed-s1".into(),
            Some("suffixed-s1".into()),
            Some("wt/suffixed-s1/work".into()),
            None,
            own.as_id().to_string(),
            None,
        )
        .await?;
    ctx.store()
        .record_worktree_lease(
            "pool".into(),
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "suffixed-s1".into(),
            Some("suffixed-s1".into()),
            None,
            None,
            other.as_id().to_string(),
            None,
        )
        .await?;

    let (status, value) = ctx
        .post(
            &format!("/v1/workers/{}/retire", worker.meta.id.as_id()),
            json!({ "force": true }),
        )
        .await;
    assert_eq!(status, 409, "{value}");
    assert!(
        value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("suffixed-s1"),
        "{value}"
    );
    let calls = ctx.drain_calls();
    assert!(
        !calls.iter().any(|call| call.method == "worker.remove"),
        "suffixed shared slot is still protected: {calls:?}"
    );

    // A different pool's slot (`other-s1`) does not resolve to this worker.
    let unrelated = ctx.seed_worker("other", None).await;
    let (status, value) = ctx
        .post(
            &format!("/v1/workers/{}/retire", unrelated.meta.id.as_id()),
            json!({ "force": true }),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    Ok(())
}

#[tokio::test]
async fn retire_without_a_lease_uses_the_legacy_remove_path() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let worker = ctx.seed_worker("plain", None).await;
    let (status, value) = ctx
        .post(
            &format!("/v1/workers/{}/retire", worker.meta.id.as_id()),
            json!({ "force": true }),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["node"]["worktreeRemoved"], true);
    let calls = ctx.drain_calls();
    assert!(
        calls.iter().any(|call| call.method == "worker.remove"),
        "unleased retire must reclaim as before: {calls:?}"
    );
    Ok(())
}

#[tokio::test]
async fn deleting_a_holder_instance_returns_its_lease_and_clears_the_lock() -> Result<()> {
    let mut ctx = Ctx::spawn().await?;
    let task = remuda_protocol::TaskId::new();
    let instance_id = remuda_protocol::InstanceId::new();

    ctx.hub.test_insert_host(&ctx.host_id).await?;
    ctx.store()
        .ensure_instance(ctx.host_id.clone(), instance_id.as_id().to_string())
        .await?;
    remuda_hub::store_test_support::settle_exited_with_task(
        ctx.store(),
        instance_id.as_id().as_str(),
        task.as_id().as_str(),
    )
    .await?;
    ctx.store()
        .record_worktree_lease(
            "pool".into(),
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "slot-del".into(),
            Some("slot-del".into()),
            Some("wt/slot-del/work".into()),
            None,
            task.as_id().to_string(),
            Some(instance_id.as_id().to_string()),
        )
        .await?;

    let (status, value) = ctx
        .delete(&format!("/v1/instances/{}", instance_id.as_id()))
        .await;
    assert_eq!(status, 200, "{value}");
    assert!(value["deleted"].as_bool().unwrap_or(false));

    let calls = ctx.drain_calls();
    let returned = calls
        .iter()
        .find(|call| call.method == "worktree.return")
        .context("delete returns the held lease")?;
    assert_eq!(returned.params["name"], "slot-del");
    assert_eq!(returned.params["taskId"], task.as_id().as_str());

    let row = ctx
        .store()
        .get_worktree_lease(
            ctx.host_id.clone(),
            ctx.workspace_id.clone(),
            "slot-del".into(),
        )
        .await?
        .expect("parked row kept after delete");
    assert_eq!(row.state, "parked");
    assert_eq!(row.refcount, 0);
    assert!(
        row.holder_instance_id.is_none(),
        "the attach lock is released with the instance"
    );
    Ok(())
}
