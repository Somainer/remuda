//! home-glance-d060 (H10): `GET /v1/owner/glance` over HTTP against the
//! in-process Hub with one fake Node.
//!
//! The unit tests in `src/owner_glance.rs` pin the rank/count rules, the row
//! caps, the deterministic redacted headline and the local-day boundary.
//! These cases pin what only the Hub wiring can show: the §5.3 scope (Main's
//! subtree plus hand-opened sessions, never another agent's private lineage),
//! the consistency of the counts with the queue, and the authority matrix.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use remuda_hub::{HubConfig, spawn};
use remuda_protocol::HostId;
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
                "capabilities": { "lineageFence": true },
                "label": format!("fake-{host_id}"),
                "host": {
                    "hostname": format!("{host_id}.local"),
                    "labels": ["toolchain=rust"],
                    "maxInstances": 8,
                    "resources": { "cpuPct": 5, "memPct": 20 },
                    "cli": [{"kind":"claude","version":"0.0.0","absolutePath":"/usr/bin/claude","authState":"unknown"}],
                    "herdr": {"version": "0.9.0", "socket": "/nonexistent/fake-herdr.sock"},
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
    data_dir: std::path::PathBuf,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    bot: String,
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
        let human = hub.mint_device_token("glance-human").await?;
        // A Bot credential: `/v1/login` with `deviceKind: "bot"`.
        let bot_login: Value = reqwest::Client::new()
            .post(format!("http://{}/v1/login", hub.addr))
            .json(&json!({ "bootstrapToken": hub.bootstrap_token,
                           "deviceName": "glance-bot", "deviceKind": "bot" }))
            .send()
            .await
            .context("bot login")?
            .json()
            .await?;
        let bot = bot_login["token"].as_str().unwrap_or_default().to_string();
        let host = HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let node = enroll_node(&hub, &host, &workspace).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Ctx {
            data_dir: dir.path().join("hub"),
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            bot,
            host,
            workspace,
            _node: node,
        })
    }

    async fn get(&self, token: &str, path: &str) -> Result<(u16, Value)> {
        let response = self
            .http
            .get(format!("{}{path}", self.base()))
            .bearer_auth(token)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
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

    async fn patch(&self, token: &str, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .patch(format!("{}{path}", self.base()))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        anyhow::ensure!(status.is_success(), "PATCH {path} {status}: {body}");
        Ok(body)
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

    async fn create_session(&self, token: &str, body: Value) -> Result<String> {
        let created = self.post(token, "/v1/instances", body).await?;
        Ok(created["instance"]["instanceId"]
            .as_str()
            .context("instance id")?
            .to_string())
    }

    async fn agent_token(&self, instance_id: &str) -> Result<String> {
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

    async fn journal(&self, instance_id: &str, event: Value) -> Result<()> {
        self.hub
            .store()
            .context("store")?
            .append_journal(self.host.clone(), instance_id.to_string(), None, event)
            .await?;
        Ok(())
    }

    /// Drive the root activity the way a shell-pty Node reports it.
    async fn activity(&self, instance_id: &str, activity: &str) -> Result<()> {
        self.journal(
            instance_id,
            json!({
                "kind": "lifecycle",
                "source": { "driverKind": "shell-pty", "channel": "hook" },
                "payload": { "type": "native", "nativeName": "PreToolUse",
                             "status": { "state": "known", "value": activity },
                             "relatedIds": { "remudaActivity": activity } }
            }),
        )
        .await
    }

    /// A settled turn end: busy → idle, which stamps `turn_end_at`.
    async fn turn_end(&self, instance_id: &str) -> Result<()> {
        self.activity(instance_id, "working").await?;
        self.activity(instance_id, "idle").await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scope_is_main_subtree_plus_hand_opened_not_private_lineages() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let main_project = ctx.project("glance-main").await?;
    let hand_project = ctx.project("glance-hand").await?;
    // Main: the address-owner seat, which may dispatch.
    let main = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "top-coordinator", "projectId": main_project,
                    "prompt": "coordinate", "driver": "claude-pty" }),
        )
        .await?;
    let main_token = ctx.agent_token(&main).await?;
    let worker = ctx
        .create_session(
            &main_token,
            json!({ "role": "worker", "projectId": main_project, "prompt": "work",
                    "driver": "claude-pty" }),
        )
        .await?;
    // A session the owner opened by hand, and its PRIVATE worker (another
    // agent's lineage: in neither Main's subtree nor hand-opened itself).
    let hand = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "project-coordinator", "projectId": hand_project,
                    "prompt": "run the other project", "driver": "claude-pty" }),
        )
        .await?;
    let hand_token = ctx.agent_token(&hand).await?;
    let private = ctx
        .create_session(
            &hand_token,
            json!({ "role": "worker", "projectId": hand_project, "prompt": "private work",
                    "driver": "claude-pty" }),
        )
        .await?;
    // A plain hand-opened session. Its task is blocked too, but the session is
    // already in the queue, so §5.1 B must not count that task as well.
    let linked = ctx
        .post(
            &ctx.human,
            "/v1/tasks",
            json!({ "projectId": hand_project, "title": "linked lane", "intent": "wait" }),
        )
        .await?;
    let linked_id = linked["id"].as_str().context("task id")?.to_string();
    ctx.patch(
        &ctx.human,
        &format!("/v1/tasks/{linked_id}"),
        json!({ "state": "failed", "reason": "same session is queued" }),
    )
    .await?;
    let plain = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "worker", "projectId": hand_project, "prompt": "just a session",
                    "taskId": linked_id, "driver": "claude-pty" }),
        )
        .await?;

    ctx.activity(&worker, "working").await?;
    ctx.activity(&private, "working").await?;
    ctx.activity(&plain, "working").await?;
    ctx.activity(&hand, "idle").await?;

    let (status, glance) = ctx.get(&ctx.human, "/v1/owner/glance").await?;
    assert_eq!(status, 200, "{glance}");
    let ids: Vec<&str> = glance["working"]
        .as_array()
        .context("working rows")?
        .iter()
        .filter_map(|row| row["instanceId"].as_str())
        .collect();
    assert!(
        ids.contains(&worker.as_str()),
        "Main's child is in scope: {ids:?}"
    );
    assert!(
        ids.contains(&plain.as_str()),
        "a hand-opened session is in scope: {ids:?}"
    );
    assert!(
        !ids.contains(&private.as_str()),
        "another agent's private lineage is out of scope: {ids:?}"
    );
    assert_eq!(glance["counts"]["working"], 2, "{glance}");

    // §5.1 N: a pending interaction plus an owner-blocked task with no such
    // interaction.
    ctx.journal(
        &plain,
        json!({ "kind": "interaction.requested", "payload": { "interaction": {
            "id": "int_glance01", "kind": "approval", "state": "pending", "blocking": true,
            "answerable": true, "carrier": "harness-hook",
            "deadline": { "state": "unknown" },
            "request": { "kind": "approval", "title": "Bash", "description": "run the tests?",
                         "toolCallId": "tc1", "options": [] } }}}),
    )
    .await?;
    let task = ctx
        .post(
            &ctx.human,
            "/v1/tasks",
            json!({ "projectId": main_project, "title": "blocked lane", "intent": "wait" }),
        )
        .await?;
    let task_id = task["id"].as_str().context("task id")?.to_string();
    ctx.patch(
        &ctx.human,
        &format!("/v1/tasks/{task_id}"),
        json!({ "state": "failed", "reason": "needs the owner's decision" }),
    )
    .await?;
    // A second blocked task whose only session is already in the queue is the
    // `linked` task above (created before `plain`).

    // A pending interaction on a session OUTSIDE the owner scope (the private
    // worker): ① needYou must still count it — §5.1 says ①, the ④ header and
    // the OS badge are one definition, and both of those read every
    // interaction. §5.3 keeps the ROW scoped, so no waiting row names it.
    ctx.journal(
        &private,
        json!({ "kind": "interaction.requested", "payload": { "interaction": {
            "id": "int_glance_private", "kind": "approval", "state": "pending", "blocking": true,
            "answerable": true, "carrier": "harness-hook",
            "deadline": { "state": "unknown" },
            "request": { "kind": "approval", "title": "Bash", "description": "private run?",
                         "toolCallId": "tc_private", "options": [] } }}}),
    )
    .await?;

    let (status, glance) = ctx.get(&ctx.human, "/v1/owner/glance").await?;
    assert_eq!(status, 200, "{glance}");
    // A (both pending approvals, one out of scope) + B (the blocked `task`,
    // whose sessions are not queued). The `linked` task is blocked too, but its
    // session already holds an approval, so it is not counted a second time
    // (§5.1).
    assert_eq!(glance["counts"]["needYou"], 3, "{glance}");
    // The queue's own source agrees: every pending row the merged page serves.
    let (status, interactions) = ctx.get(&ctx.human, "/v1/interactions").await?;
    assert_eq!(status, 200, "{interactions}");
    let pending_rows = interactions["items"]
        .as_array()
        .context("interactions items")?
        .iter()
        .filter(|item| item["state"].as_str() == Some("pending"))
        .count();
    assert_eq!(
        u64::try_from(pending_rows).unwrap_or(u64::MAX) + 1,
        glance["counts"]["needYou"].as_u64().unwrap_or(0),
        "needYou == the queue's interactions + B: {glance} {interactions}"
    );
    let waiting = glance["waiting"].as_array().context("waiting rows")?;
    let row = waiting
        .iter()
        .find(|row| row["instanceId"].as_str() == Some(plain.as_str()))
        .context("the blocked session has a waiting row")?;
    assert_eq!(row["kind"], "approval");
    assert!(
        row["headline"]
            .as_str()
            .is_some_and(|text| text.contains("run the tests?")),
        "{row}"
    );
    assert!(
        waiting
            .iter()
            .all(|row| row["instanceId"].as_str() != Some(private.as_str())),
        "the out-of-scope session gets no waiting row: {waiting:?}"
    );
    Ok(())
}

/// [r4 item 1] A paused Main must not drop its subtree: the owner taps 暂停
/// (the close of a continuity lineage = F without successor, which fences the
/// chapter) while a worker Main dispatched keeps working. The Main seat still
/// resolves (home.md §5.3, the shared `main_seat_cases`), so the worker is
/// still counted in ① and listed in ⑥'s rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_main_keeps_its_working_subtree_in_the_glance() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("glance-paused-main").await?;
    let main = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "top-coordinator", "projectId": project,
                    "prompt": "coordinate", "driver": "claude-pty" }),
        )
        .await?;
    let main_token = ctx.agent_token(&main).await?;
    let worker = ctx
        .create_session(
            &main_token,
            json!({ "role": "worker", "projectId": project, "prompt": "work",
                    "driver": "claude-pty" }),
        )
        .await?;
    ctx.activity(&worker, "working").await?;

    let working_ids = |glance: &Value| -> Vec<String> {
        glance["working"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["instanceId"].as_str().map(str::to_string))
            .collect()
    };
    let (status, before) = ctx.get(&ctx.human, "/v1/owner/glance").await?;
    assert_eq!(status, 200, "{before}");
    assert!(working_ids(&before).contains(&worker), "{before}");

    // 暂停: the close of a continuity lineage's chapter fences it.
    let response = ctx
        .http
        .post(format!("{}/v1/instances/{main}/commands", ctx.base()))
        .bearer_auth(&ctx.human)
        .json(&json!({ "operation": "instance.close", "payload": {} }))
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await?;
    assert!(status.is_success(), "pause {status}: {body}");
    assert_eq!(body["paused"], true, "{body}");
    let db = rusqlite::Connection::open(ctx.data_dir.join("hub.sqlite"))?;
    let fenced: Option<String> = db.query_row(
        "SELECT fenced_at FROM instances WHERE id = ?1",
        rusqlite::params![main],
        |row| row.get(0),
    )?;
    assert!(fenced.is_some(), "the pause fenced Main's chapter");
    drop(db);

    let (status, after) = ctx.get(&ctx.human, "/v1/owner/glance").await?;
    assert_eq!(status, 200, "{after}");
    assert!(
        working_ids(&after).contains(&worker),
        "the paused Main's working worker is still listed: {after}"
    );
    assert_eq!(
        after["counts"]["working"], before["counts"]["working"],
        "and still counted: {before} → {after}"
    );
    Ok(())
}

/// [r4 item 2] A parent means "dispatched by an Agent", never "resumed". A
/// D-026 resume writes `parentInstanceId` = the resumed session (http.rs), so
/// the successor's creator stamp is the chain root's — not "has a parent" and
/// not the operator who pressed Resume. A resumed conversation stays the
/// owner's (`human`, in the glance); a resumed worker stays its Agent's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_inherits_the_chain_roots_creator_stamp() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("glance-resume-origin").await?;
    let coordinator = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "project-coordinator", "projectId": project,
                    "prompt": "coordinate", "driver": "claude-pty" }),
        )
        .await?;
    let coordinator_token = ctx.agent_token(&coordinator).await?;
    let conversation = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "worker", "projectId": project, "prompt": "talk",
                    "driver": "claude-pty" }),
        )
        .await?;
    let worker = ctx
        .create_session(
            &coordinator_token,
            json!({ "role": "worker", "projectId": project, "prompt": "work",
                    "driver": "claude-pty" }),
        )
        .await?;
    let mut resumed = Vec::new();
    for (index, id) in [&conversation, &worker].into_iter().enumerate() {
        ctx.journal(
            id,
            json!({ "kind": "lifecycle", "payload": {
                "type": "native", "topic": "session", "nativeName": "session",
                "nativeId": { "state": "known",
                              "value": format!("01993ab0-0000-7000-8000-00000000000{index}") },
                "status": { "state": "known", "value": "started" },
                "relatedIds": {}, "dataRef": null, "severity": "info",
                "affectsCompletion": false } }),
        )
        .await?;
        ctx.journal(
            id,
            json!({ "kind": "lifecycle", "payload": {
                "type": "entity", "state": "exited", "reasonCode": "native-exit" } }),
        )
        .await?;
        // The OWNER presses Resume on both.
        let body = ctx
            .post(
                &ctx.human,
                &format!("/v1/instances/{id}/resume"),
                json!({ "mode": "structured" }),
            )
            .await?;
        let successor = &body["instance"];
        assert_eq!(successor["resumedFrom"], json!(id), "{body}");
        assert_eq!(
            successor["parentInstanceId"],
            json!(id),
            "the D-026 shape names the resumed session as parent: {body}"
        );
        resumed.push(
            successor["instanceId"]
                .as_str()
                .context("successor id")?
                .to_string(),
        );
    }
    let (status, view) = ctx
        .get(&ctx.human, &format!("/v1/instances/{}", resumed[0]))
        .await?;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        view["creatorOrigin"], "human",
        "a resumed conversation is still the owner's: {view}"
    );
    let (status, view) = ctx
        .get(&ctx.human, &format!("/v1/instances/{}", resumed[1]))
        .await?;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        view["creatorOrigin"], "agent",
        "a resumed worker is still its Agent's, though a human pressed Resume: {view}"
    );

    ctx.activity(&resumed[0], "working").await?;
    let (status, glance) = ctx.get(&ctx.human, "/v1/owner/glance").await?;
    assert_eq!(status, 200, "{glance}");
    let listed = glance["working"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|row| row["instanceId"].as_str() == Some(resumed[0].as_str()));
    assert!(listed, "the resumed conversation is in ⑥: {glance}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_owners_own_human_device_may_read_the_glance() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("glance-authority").await?;
    let main = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "top-coordinator", "projectId": project,
                    "prompt": "coordinate", "driver": "claude-pty" }),
        )
        .await?;
    let agent = ctx.agent_token(&main).await?;
    assert_eq!(ctx.get(&ctx.human, "/v1/owner/glance").await?.0, 200);
    assert_eq!(
        ctx.get(&agent, "/v1/owner/glance").await?.0,
        403,
        "an Agent seat"
    );
    if !ctx.bot.is_empty() {
        assert_eq!(ctx.get(&ctx.bot, "/v1/owner/glance").await?.0, 403, "a Bot");
    }
    // A Human credential narrowed to a seat is an Agent (agent_scope::caller).
    let response = ctx
        .http
        .get(format!("{}/v1/owner/glance", ctx.base()))
        .bearer_auth(&ctx.human)
        .header("x-remuda-instance-id", &main)
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 403, "a narrowed Human");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_today_counts_a_settled_turn_in_the_callers_local_day() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let project = ctx.project("glance-today").await?;
    let session = ctx
        .create_session(
            &ctx.human,
            json!({ "role": "worker", "projectId": project, "prompt": "finish a turn",
                    "driver": "claude-pty" }),
        )
        .await?;
    ctx.turn_end(&session).await?;
    // A settled turn that just ended is inside "today" for every real offset.
    for offset in [-720, 0, 840] {
        let (status, glance) = ctx
            .get(
                &ctx.human,
                &format!("/v1/owner/glance?tzOffsetMinutes={offset}"),
            )
            .await?;
        assert_eq!(status, 200, "{glance}");
        assert_eq!(glance["timezoneOffsetMinutes"], offset, "{glance}");
        assert_eq!(
            glance["counts"]["doneToday"], 1,
            "offset {offset}: {glance}"
        );
    }
    // Out-of-range offsets are clamped, not refused.
    let (status, glance) = ctx
        .get(&ctx.human, "/v1/owner/glance?tzOffsetMinutes=9999")
        .await?;
    assert_eq!(status, 200);
    assert_eq!(glance["timezoneOffsetMinutes"], 840);
    Ok(())
}
