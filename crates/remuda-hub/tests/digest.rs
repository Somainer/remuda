//! cb-digest: `GET /v1/workers/{id}/digest` and `GET /v1/lineages/{id}/glance`
//! over HTTP against the in-process Hub with one fake Node.
//!
//! The unit tests in `src/digest.rs` pin the rendering rules (byte budget,
//! redaction, folding). These cases pin what only the Hub wiring can show: the
//! digest is assembled from real stored rows (journal tail, interaction row,
//! command ledger, provider supply), and the lineage read predicate admits an
//! Agent for its own lineage's workers only, without widening `/v1/workers/*`.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::{HostId, WORKER_DIGEST_MAX_BYTES};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

struct FakeNode {
    task: tokio::task::JoinHandle<()>,
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
    workspace: &str,
) -> Result<FakeNode> {
    let enroll = hub
        .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
        .await?;
    let mut request = format!("ws://{}/v1/node", hub.addr).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {enroll}").parse()?);
    let (mut node, _) = tokio_tungstenite::connect_async(request).await?;
    node.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": "hello", "method": "runtime.hello",
            "params": {
                "hostId": host_id,
                "nodeVersion": "0.1.0-test",
                "label": format!("fake-{host_id}"),
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "labels": ["toolchain=rust"],
                    "maxInstances": 8,
                    "resources": { "cpuPct": 5, "memPct": 20 },
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr": {"version": "0.9.0", "socket": "/tmp/fake-herdr.sock"},
                    "workspaces": [{ "workspaceId": workspace, "hostId": host_id,
                                     "root": format!("/tmp/{workspace}") }],
                    "workspaceRevision": 1,
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
                let mut response = json!({ "result": { "accepted": true } });
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

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
    workspace: String,
    _node: FakeNode,
}

impl Ctx {
    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("digest-human").await?;
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = enroll_node(&hub, &host, &workspace).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
            workspace,
            _node: node,
        })
    }

    /// GET `path` as `token`; returns the status and the raw body bytes.
    async fn get(&self, token: &str, path: &str) -> Result<(u16, Vec<u8>)> {
        let response = self
            .http
            .get(format!("{}{path}", self.base()))
            .bearer_auth(token)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.bytes().await?.to_vec()))
    }

    /// GET `path` as `token` with one extra caller header.
    async fn get_with_header(
        &self,
        token: &str,
        path: &str,
        name: &str,
        value: &str,
    ) -> Result<(u16, Vec<u8>)> {
        let response = self
            .http
            .get(format!("{}{path}", self.base()))
            .bearer_auth(token)
            .header(name, value)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.bytes().await?.to_vec()))
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{path}", self.base()))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        anyhow::ensure!(status.is_success(), "POST {path} {status}: {body}");
        Ok(body)
    }

    async fn agent_token_for(&self, instance_id: &str) -> Result<String> {
        let value = self
            .post(
                &self.human,
                &format!("/v1/instances/{instance_id}/mcp-token"),
                json!({}),
            )
            .await?;
        value["token"]
            .as_str()
            .map(str::to_string)
            .context("mcp token")
    }

    async fn project(&self, name: &str) -> Result<String> {
        let created = self
            .post(
                &self.human,
                "/v1/projects",
                json!({ "name": name, "members": [
                    {"hostId": self.host, "workspaceId": self.workspace, "role": "primary"}] }),
            )
            .await?;
        Ok(created["id"].as_str().context("project id")?.to_string())
    }

    /// A human-seated project coordinator; returns (instance id, agent token).
    async fn coordinator(&self, project: &str) -> Result<(String, String)> {
        let created = self
            .post(
                &self.human,
                "/v1/instances",
                json!({ "role": "project-coordinator", "projectId": project,
                        "prompt": "coordinate", "driver": "claude-pty" }),
            )
            .await?;
        let id = created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string();
        let token = self.agent_token_for(&id).await?;
        Ok((id, token))
    }

    /// A worker instance delegated by `token`'s coordinator, plus its roster row.
    async fn worker_under(
        &self,
        token: &str,
        project: &str,
        name: &str,
        provider_profile_id: Option<&str>,
    ) -> Result<(String, String)> {
        let created = self
            .post(
                token,
                "/v1/instances",
                json!({ "role": "worker", "projectId": project, "prompt": "work",
                        "driver": "claude-pty" }),
            )
            .await?;
        let instance_id = created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string();
        let roster_id = remuda_protocol::WorkerRosterId::new().as_id().to_string();
        let mut doc = json!({
            "id": roster_id, "revision": "1",
            "createdAt": "2026-10-29T01:00:00.000Z", "updatedAt": "2026-10-29T01:00:00.000Z",
            "projectId": project, "name": name, "instanceId": instance_id,
            "hostId": self.host, "workspaceId": self.workspace, "harness": "claude",
            "driver": "claude-sdk", "model": "claude-sonnet-5-5",
            "branch": format!("wt/{name}/b-digest-md"), "worktreePath": format!("/w/{name}"),
            "state": { "state": "working" },
        });
        if let Some(profile) = provider_profile_id {
            doc["providerProfileId"] = json!(profile);
        }
        let worker: remuda_protocol::WorkerRoster = serde_json::from_value(doc)?;
        self.hub
            .store()
            .context("store")?
            .insert_worker(worker, "digest-test".into())
            .await?;
        Ok((roster_id, instance_id))
    }

    /// A DONE worker under `token`'s coordinator. `done_at` is the day it
    /// reported; `None` leaves the stamp to the insert (today).
    async fn done_worker(
        &self,
        token: &str,
        project: &str,
        name: &str,
        done_at: Option<&str>,
    ) -> Result<(String, String)> {
        let created = self
            .post(
                token,
                "/v1/instances",
                json!({ "role": "worker", "projectId": project, "prompt": "work",
                        "driver": "claude-pty" }),
            )
            .await?;
        let instance_id = created["instance"]["instanceId"]
            .as_str()
            .context("id")?
            .to_string();
        let roster_id = remuda_protocol::WorkerRosterId::new().as_id().to_string();
        let mut doc = json!({
            "id": roster_id, "revision": "1",
            "createdAt": "2026-10-29T01:00:00.000Z", "updatedAt": "2026-10-29T01:00:00.000Z",
            "projectId": project, "name": name, "instanceId": instance_id,
            "hostId": self.host, "workspaceId": self.workspace, "harness": "claude",
            "driver": "claude-sdk", "model": "claude-sonnet-5-5",
            "branch": format!("wt/{name}/b-digest-md"), "worktreePath": format!("/w/{name}"),
            "state": { "state": "done", "sha": "0a10ebf351aa" },
        });
        if let Some(at) = done_at {
            doc["doneAt"] = json!(at);
        }
        let worker: remuda_protocol::WorkerRoster = serde_json::from_value(doc)?;
        self.hub
            .store()
            .context("store")?
            .insert_worker(worker, "digest-test".into())
            .await?;
        Ok((roster_id, instance_id))
    }

    async fn journal(&self, instance_id: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        Ok(())
    }
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("response is valid JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_is_assembled_from_stored_rows_and_stays_within_budget() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("digest").await?;
    let (_coordinator, token) = ctx.coordinator(&project).await?;

    // A parked supply: a real provider profile with an exhausted weekly window.
    let profile = ctx
        .post(
            &ctx.human,
            "/v1/providers",
            json!({ "name": "relay", "kind": "gateway", "baseUrl": "http://127.0.0.1:1",
                    "authToken": "sk-digest-test-secret-aaaa",
                    "models": [{ "id": "claude-sonnet-5-5" }] }),
        )
        .await?;
    let profile_id = profile["id"].as_str().context("profile id")?.to_string();
    ctx.post(
        &ctx.human,
        &format!("/v1/providers/{profile_id}/supply/events"),
        json!({ "type": "structured", "windows": [{
            "id": "weekly", "appliesTo": ["*"], "windowDurationMins": 10080,
            "usedPercent": 100, "resetsAt": 9_999_999_999_i64 }] }),
    )
    .await?;

    let (worker_id, instance_id) = ctx
        .worker_under(&token, &project, "c-digest", Some(&profile_id))
        .await?;

    // Journal: a long CJK assistant message, a Bash call that needs approval.
    let wall = "字".repeat(30_000);
    ctx.journal(
        &instance_id,
        json!({ "kind": "message", "payload": { "operation": "open", "messageId": "m1",
            "role": "assistant", "parentToolCallId": null,
            "blocks": [{ "type": "text", "text": wall }] }}),
    )
    .await?;
    ctx.journal(
        &instance_id,
        json!({ "kind": "tool_call", "payload": { "operation": "open", "toolCallId": "tc1",
            "toolName": { "state": "known", "value": "Bash" },
            "input": { "state": "known", "value": {
                "command": "sleep 120 && curl -H 'X-Api-Key: agk_9f8e7d6c5b4a39281706' \
                            --header=Bearer q7Zr2LmW8pXc4VbN1 https://x.test",
                "handoff": "owner-typed-handoff-9917" } },
            "state": "proposed" }}),
    )
    .await?;
    ctx.journal(
        &instance_id,
        json!({ "kind": "interaction.requested", "payload": { "interaction": {
            "id": "int_digest01", "kind": "approval", "state": "pending", "blocking": true,
            "answerable": true, "carrier": "harness-hook",
            "request": { "kind": "approval", "title": "Bash", "description": "Run a long command?",
                         "toolCallId": "tc1", "options": [] } }}}),
    )
    .await?;
    // An effort switch still queued at the Hub, with its request id.
    ctx.hub
        .store()
        .context("store")?
        .queue_command(
            Some("cmd_digest_configure".into()),
            Some(instance_id.clone()),
            ctx.host.clone(),
            "instance.configure".into(),
            json!({ "effort": { "name": "high", "requestId": "effreq-digest" } }),
            None,
            None,
        )
        .await?;

    let (status, bytes) = ctx
        .get(&token, &format!("/v1/workers/{worker_id}/digest"))
        .await?;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert!(
        bytes.len() <= WORKER_DIGEST_MAX_BYTES,
        "{} bytes",
        bytes.len()
    );
    let digest = json_of(&bytes);
    assert_eq!(digest["v"], 1);
    assert_eq!(digest["workerId"], worker_id);
    assert_eq!(digest["name"], "c-digest");
    assert_eq!(digest["roster"]["state"], "working");
    assert!(
        digest["shrunk"].as_array().is_some_and(|s| !s.is_empty()),
        "30k CJK characters cannot fit"
    );
    assert_eq!(digest["toolInFlight"], true);
    assert_eq!(digest["pending"]["kind"], "approval");
    assert_eq!(digest["pending"]["tool"], "Bash");
    assert_eq!(digest["pending"]["question"], "Run a long command?");
    let args = digest["pending"]["argsSummary"].as_str().context("args")?;
    assert!(args.starts_with("command=sleep 120"), "{args}");
    // The same candidate set scripts/ci/secret-scan.py flags, including
    // Remuda's own agent-token prefix and a token that rides a Bearer scheme.
    assert!(
        !args.contains("sk-ant-api03")
            && !args.contains("agk_9f8e7d6c5b4a39281706")
            && !args.contains("q7Zr2LmW8pXc4VbN1")
            && !args.contains("owner-typed-handoff"),
        "{args}"
    );
    assert!(args.contains("handoff=[withheld]"), "{args}");
    assert_eq!(digest["model"]["configure"]["state"], "queued");
    assert_eq!(digest["model"]["configure"]["requestId"], "effreq-digest");
    assert_eq!(digest["supply"]["resetsAt"], 9_999_999_999_i64);
    assert!(digest["queued"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(digest["model"]["requested"], "claude-sonnet-5-5");
    assert!(digest["sinceProgressSecs"].is_number());
    assert_eq!(digest["branch"], "wt/c-digest/b-digest-md");

    // The operator reads the same document, addressed by worker name.
    let (status, by_name) = ctx.get(&ctx.human, "/v1/workers/c-digest/digest").await?;
    assert_eq!(status, 200);
    assert_eq!(json_of(&by_name)["workerId"], worker_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_off_wire_pending_kind_keeps_the_response_within_budget() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("digest-kind").await?;
    let (_coordinator, token) = ctx.coordinator(&project).await?;
    let (worker_id, instance_id) = ctx.worker_under(&token, &project, "c-kind", None).await?;
    // A Node journals untyped JSON, so the kind can be any length it likes.
    let wall = "k".repeat(3000);
    ctx.journal(
        &instance_id,
        json!({ "kind": "interaction.requested", "payload": { "interaction": {
            "id": "int_kind", "kind": wall, "state": "pending", "blocking": true,
            "request": { "kind": wall, "title": "T", "description": "d", "options": [] } }}}),
    )
    .await?;
    let (status, bytes) = ctx
        .get(&token, &format!("/v1/workers/{worker_id}/digest"))
        .await?;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert!(
        bytes.len() <= WORKER_DIGEST_MAX_BYTES,
        "{} bytes",
        bytes.len()
    );
    let digest = json_of(&bytes);
    assert_eq!(digest["pending"]["kind"], "unknown");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_today_counts_the_day_a_worker_reported_done_not_its_last_write() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("digest-done").await?;
    let (coordinator, token) = ctx.coordinator(&project).await?;
    // Reported DONE long ago, then re-observed (watched) today.
    let (stale, _) = ctx
        .done_worker(
            &token,
            &project,
            "c-stale",
            Some("2020-01-02T03:04:05.000Z"),
        )
        .await?;
    // Reported DONE today, with the stamp written by the transition.
    let (fresh, _) = ctx.done_worker(&token, &project, "c-fresh", None).await?;
    let glance_path = format!("/v1/lineages/{coordinator}/glance");

    let glance = json_of(&ctx.get(&token, &glance_path).await?.1);
    assert_eq!(
        glance["doneToday"], 1,
        "only the worker that reported done today counts: {glance}"
    );

    // Every watch pass rewrites the roster row and its updated_at. A DONE
    // worker watched today must keep the day it actually reported.
    let store = ctx.hub.store().context("store")?;
    for id in [&stale, &fresh] {
        store
            .mutate_worker(id.to_string(), |_| Ok(()))
            .await
            .context("re-observe")?;
    }
    let glance = json_of(&ctx.get(&token, &glance_path).await?.1);
    assert_eq!(
        glance["doneToday"], 1,
        "a re-observed DONE worker keeps its day: {glance}"
    );

    // A worker that leaves DONE and reports again today counts again.
    store
        .mutate_worker(stale.clone(), |row| {
            row.state = remuda_protocol::WorkerState::Working;
            Ok(())
        })
        .await
        .context("reopen")?;
    store
        .mutate_worker(stale, |row| {
            row.state = remuda_protocol::WorkerState::Done {
                sha: "0a10ebf351aa".into(),
            };
            Ok(())
        })
        .await
        .context("re-done")?;
    let glance = json_of(&ctx.get(&token, &glance_path).await?.1);
    assert_eq!(
        glance["doneToday"], 2,
        "a fresh DONE today counts: {glance}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_reads_only_its_own_lineages_workers_and_nothing_else_under_workers() -> Result<()>
{
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("digest-a").await?;
    let project_b = ctx.project("digest-b").await?;
    let (coordinator_a, token_a) = ctx.coordinator(&project).await?;
    let (coordinator_b, token_b) = ctx.coordinator(&project_b).await?;
    let (worker_a, instance_a) = ctx
        .worker_under(&token_a, &project, "c-owned", None)
        .await?;
    let digest_path = format!("/v1/workers/{worker_a}/digest");

    // Own lineage: admitted.
    assert_eq!(ctx.get(&token_a, &digest_path).await?.0, 200);
    // Another lineage: refused, by id and by name.
    assert_eq!(ctx.get(&token_b, &digest_path).await?.0, 403);
    assert_eq!(
        ctx.get(&token_b, "/v1/workers/c-owned/digest").await?.0,
        403
    );
    // A name that does not exist and an id that does not exist are not told
    // apart from a foreign one by status class for an Agent.
    assert_eq!(
        ctx.get(&token_b, "/v1/workers/wkr_unknown/digest").await?.0,
        403
    );
    // The route family is not widened: roster reads stay refused to Agents.
    for path in [
        "/v1/workers".to_string(),
        format!("/v1/workers/{worker_a}"),
        format!("/v1/workers/{worker_a}/nudge"),
    ] {
        assert_eq!(ctx.get(&token_a, &path).await?.0, 403, "{path}");
    }
    // A leaf worker holds no `dispatch` grant (project.rs:35), so even its own
    // digest is refused — the same rule as every other scope-wide worker read
    // (D-060 §8.4: require the grant and re-check the caller's project scope).
    let worker_token = ctx.agent_token_for(&instance_a).await?;
    assert_eq!(ctx.get(&worker_token, &digest_path).await?.0, 403);
    let (sibling, _) = ctx
        .worker_under(&token_a, &project, "c-sibling", None)
        .await?;
    assert_eq!(
        ctx.get(&worker_token, &format!("/v1/workers/{sibling}/digest"))
            .await?
            .0,
        403
    );

    // A Bot device reads as an operator, but it is a courier: presenting the
    // caller header would let it borrow an Agent's scope, so it is refused
    // before the narrowing. The decision is made on the authenticated device.
    let bot = ctx
        .post(
            "unused-bearer",
            "/v1/login",
            json!({ "bootstrapToken": ctx.hub.bootstrap_token,
                "deviceName": "digest-bot", "deviceKind": "bot" }),
        )
        .await?["token"]
        .as_str()
        .context("bot token")?
        .to_string();
    assert_eq!(ctx.get(&bot, &digest_path).await?.0, 200);
    let (status, _) = ctx
        .get_with_header(&bot, &digest_path, "x-remuda-instance-id", &coordinator_a)
        .await?;
    assert_eq!(status, 403, "a Bot must not narrow itself into Agent scope");

    // Glance: a lineage lists the workers it dispatched; waiting vs working.
    ctx.journal(
        &instance_a,
        json!({ "kind": "interaction.requested", "payload": { "interaction": {
            "id": "int_glance01", "kind": "approval", "state": "pending", "blocking": true,
            "request": { "kind": "approval", "title": "Bash", "description": "ok?",
                         "toolCallId": "tc1", "options": [] } }}}),
    )
    .await?;
    let glance_path = format!("/v1/lineages/{coordinator_a}/glance");
    let (status, bytes) = ctx.get(&token_a, &glance_path).await?;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let glance = json_of(&bytes);
    assert_eq!(glance["lineageId"], coordinator_a);
    assert!(glance["generatedAt"].is_string());
    assert_eq!(glance["more"]["waiting"], 0);
    assert_eq!(glance["waiting"][0]["name"], "c-owned");
    assert_eq!(glance["waiting"][0]["kind"], "approval");
    assert_eq!(glance["waiting"][0]["question"], "Bash");
    assert_eq!(glance["more"]["working"], 0);
    assert_eq!(glance["working"][0]["name"], "c-sibling");
    assert_eq!(glance["doneToday"], 0);
    // Another lineage's Agent is refused; its own glance is empty.
    assert_eq!(ctx.get(&token_b, &glance_path).await?.0, 403);
    let own_b = json_of(
        &ctx.get(&token_b, &format!("/v1/lineages/{coordinator_b}/glance"))
            .await?
            .1,
    );
    assert_eq!(
        (
            own_b["more"]["waiting"].as_u64(),
            own_b["more"]["working"].as_u64()
        ),
        (Some(0), Some(0))
    );
    // The operator reads any lineage; an unknown one is a 404.
    assert_eq!(ctx.get(&ctx.human, &glance_path).await?.0, 200);
    assert_eq!(
        ctx.get(&ctx.human, "/v1/lineages/ins_unknown/glance")
            .await?
            .0,
        404
    );
    // The glance carries the same grant and courier rules as the digest: a
    // leaf has no `dispatch` grant, and a Bot may not narrow itself.
    assert_eq!(ctx.get(&worker_token, &glance_path).await?.0, 403);
    assert_eq!(ctx.get(&bot, &glance_path).await?.0, 200);
    let (status, _) = ctx
        .get_with_header(&bot, &glance_path, "x-remuda-instance-id", &coordinator_a)
        .await?;
    assert_eq!(status, 403, "a Bot must not narrow itself into Agent scope");
    // The lineage read itself is unchanged for the owner, refused for B.
    assert_eq!(
        ctx.get(&token_a, &format!("/v1/lineages/{coordinator_a}"))
            .await?
            .0,
        200
    );
    assert_eq!(
        ctx.get(&token_b, &format!("/v1/lineages/{coordinator_a}"))
            .await?
            .0,
        403
    );
    // /v1/caller names the caller's lineage so `remuda glance` can default it.
    let caller = json_of(&ctx.get(&token_a, "/v1/caller").await?.1);
    assert_eq!(caller["lineageId"], coordinator_a);
    Ok(())
}
