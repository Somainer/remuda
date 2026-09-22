//! D-051 (c-deleg2): truthful answer actor + the unconditional
//! `interaction.answered` audit row.
//!
//! Every successful answer CAS — a Node-ownered durable interaction or the
//! Hub's in-memory one-shot approval — leaves exactly one
//! `interaction.answered` row whose `detail_json` carries the delegation
//! chain (`chain[]`), the answerer's integer level in it, and the device
//! origin truthfulness (`byDevice`/`byInstanceId`/`byOrigin`/`interactionKind`).
//! The bot-relay `interaction.bot-answer` row stays additive.
//!
//! All fixtures are scripted directly against the Hub + SQLite (no real
//! Node): pending rows are plain `interactions` inserts, and every successful
//! answer mounts a synthetic Node transport.

use anyhow::{Context, Result};
use remuda_hub::store_test_support::Store;
use remuda_hub::{HubConfig, HubError, NodeTransport, TransportKind, spawn};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tempfile::TempDir;

const NOW: &str = "2026-09-22T00:00:00.000Z";

// The D-051 switch override is process-global; this whole binary keeps it on.
static FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct FlagGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

async fn flag_on() -> FlagGuard {
    let guard = FLAG_LOCK.lock().await;
    remuda_hub::delegated_decisions_test_support::set_global_override(Some(true));
    FlagGuard { _guard: guard }
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        remuda_hub::delegated_decisions_test_support::set_global_override(None);
    }
}

/// Synthetic Node transport that records every frame the Hub sends and always
/// returns the scripted success frame.
struct RecordingTransport {
    calls: StdMutex<Vec<(String, Value)>>,
    reply: Option<Value>,
}

impl RecordingTransport {
    fn accepted() -> Arc<Self> {
        Arc::new(Self {
            calls: StdMutex::new(Vec::new()),
            reply: Some(json!({
                "jsonrpc": "2.0", "id": "1",
                "result": { "outcome": "accepted", "state": "answered" }
            })),
        })
    }

    fn frame(&self, method: &str) -> Value {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(m, _)| m == method)
            .map(|(_, params)| params.clone())
            .expect("expected node rpc frame")
    }
}

impl NodeTransport for RecordingTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        let reply = self.reply.clone();
        Box::pin(async move { Ok(reply) })
    }

    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

struct Ctx {
    _dir: TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    store: Store,
    host: String,
    node: Arc<RecordingTransport>,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let human = hub.mint_device_token("dd2-human").await?;
        let host = remuda_protocol::HostId::new().as_id().as_str().to_string();
        hub.test_insert_host(&host).await?;
        let node = RecordingTransport::accepted();
        hub.test_set_node_transport(&host, node.clone()).await;
        let store = hub.store().context("hub store")?.clone();
        Ok(Self {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            store,
            host,
            node,
        })
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        Ok(rusqlite::Connection::open(
            self._dir.path().join("data").join("hub.sqlite"),
        )?)
    }

    /// Insert an instance, stamping the immutable `parentInstanceId` edge into
    /// its spec JSON directly — the same fixture shape c-deleg1 uses.
    async fn instance(&self, mode: &str, parent: Option<&str>) -> Result<String> {
        let record = self
            .store
            .insert_instance(
                self.host.clone(),
                None,
                "claude".into(),
                "claude-pty".into(),
                None,
                json!({ "permissionMode": mode, "kind": "claude" }),
            )
            .await?;
        if let Some(parent) = parent {
            self.db()?.execute(
                "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
                rusqlite::params![
                    json!({ "permissionMode": mode, "kind": "claude",
                             "parentInstanceId": parent })
                    .to_string(),
                    record.instance_id
                ],
            )?;
        }
        Ok(record.instance_id)
    }

    fn interaction(&self, instance_id: &str, kind: &str) -> Result<String> {
        let id = remuda_protocol::InteractionId::new()
            .as_id()
            .as_str()
            .to_string();
        self.db()?.execute(
            "INSERT INTO interactions
                (id, instance_id, host_id, kind, state, blocking, payload_json,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 1, '{}', ?5, ?5)",
            rusqlite::params![id, instance_id, self.host, kind, NOW],
        )?;
        Ok(id)
    }

    async fn agent_token(&self, instance_id: &str) -> Result<String> {
        self.hub
            .test_mint_agent_token(&format!("agent-{instance_id}"), instance_id)
            .await
    }

    fn device_id(&self, name: &str) -> Result<String> {
        Ok(self.db()?.query_row(
            "SELECT id FROM devices WHERE name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )?)
    }

    async fn answer(
        &self,
        token: &str,
        interaction_id: &str,
        body: Value,
    ) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!(
                "{}/v1/interactions/{interaction_id}/answer",
                self.base()
            ))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?)
    }

    async fn answer_question(&self, token: &str, interaction_id: &str) -> Result<reqwest::Response> {
        self.answer(
            token,
            interaction_id,
            json!({ "answer": { "kind": "question", "answers": {} } }),
        )
        .await
    }

    /// `SELECT … FROM audit_log WHERE subject=? ORDER BY id ASC` — the exact
    /// reconstruction query the evidence doc quotes.
    fn audits(&self, subject: &str) -> Result<Vec<(String, Value)>> {
        let rows = self.db()?.prepare(
            "SELECT action, detail_json FROM audit_log
             WHERE subject = ?1 ORDER BY id ASC",
        )?.query_map(rusqlite::params![subject], |row| {
            let action: String = row.get(0)?;
            let raw: String = row.get(1)?;
            let detail = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
            Ok((action, detail))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn answered_detail(&self, interaction_id: &str) -> Result<Value> {
        let rows = self.audits(interaction_id)?;
        let (action, detail) = rows
            .iter()
            .find(|(action, _)| action == "interaction.answered")
            .expect("interaction.answered audit row");
        assert_eq!(action, "interaction.answered");
        Ok(detail.clone())
    }
}

fn chain_ids(detail: &Value) -> Vec<String> {
    detail["chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| hop["instanceId"].as_str().unwrap().to_string())
        .collect()
}

// --- answeredByLevel: the three acceptance levels --------------------------

#[tokio::test]
async fn audit_answered_by_level_zero_when_self() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let question = ctx.interaction(&root, "question")?;
    let token = ctx.agent_token(&root).await?;

    let response = ctx.answer_question(&token, &question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    let detail = ctx.answered_detail(&question)?;
    assert_eq!(chain_ids(&detail), vec![root.clone()]);
    assert_eq!(detail["chain"][0]["parentInstanceId"], Value::Null);
    assert_eq!(detail["answeredByLevel"], 0);
    assert_eq!(detail["byOrigin"], "agent");
    assert_eq!(detail["byInstanceId"], json!(root));
    assert_eq!(detail["byDevice"], json!(ctx.device_id(&format!("agent-{root}"))?));
    assert_eq!(detail["interactionKind"], "question");
    Ok(())
}

#[tokio::test]
async fn audit_answered_by_level_one_when_direct_parent() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&root)).await?;
    let question = ctx.interaction(&child, "question")?;
    let token = ctx.agent_token(&root).await?;

    let response = ctx.answer_question(&token, &question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    let detail = ctx.answered_detail(&question)?;
    assert_eq!(chain_ids(&detail), vec![child.clone(), root.clone()]);
    assert_eq!(detail["chain"][0]["parentInstanceId"], json!(root));
    assert_eq!(detail["answeredByLevel"], 1);
    assert_eq!(detail["byOrigin"], "agent");
    assert_eq!(detail["byInstanceId"], json!(root));
    Ok(())
}

#[tokio::test]
async fn audit_answered_by_level_last_when_root_operator() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    // Three-level tree: the human operator at the root answers an interaction
    // a grandchild agent is blocking on.
    let root = ctx.instance("manual", None).await?;
    let middle = ctx.instance("manual", Some(&root)).await?;
    let leaf = ctx.instance("manual", Some(&middle)).await?;
    let question = ctx.interaction(&leaf, "question")?;

    let response = ctx.answer_question(&ctx.human, &question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    let detail = ctx.answered_detail(&question)?;
    assert_eq!(
        chain_ids(&detail),
        vec![leaf.clone(), middle.clone(), root.clone()]
    );
    assert_eq!(detail["answeredByLevel"], 2);
    assert_eq!(detail["answeredByLevel"], detail["chain"].as_array().unwrap().len() as i64 - 1);
    assert_eq!(detail["byOrigin"], "human");
    // An operator device carries no bound instance.
    assert_eq!(detail["byInstanceId"], Value::Null);
    assert_eq!(detail["byDevice"], json!(ctx.device_id("dd2-human")?));
    Ok(())
}

// --- one unconditional row; bot-relay row stays additive -------------------

#[tokio::test]
async fn plain_human_answer_writes_one_answered_row_and_no_bot_relay_row() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let question = ctx.interaction(&root, "question")?;

    let response = ctx.answer_question(&ctx.human, &question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    let rows = ctx.audits(&question)?;
    assert_eq!(rows.len(), 1, "exactly one audit row: {rows:?}");
    assert_eq!(rows[0].0, "interaction.answered");
    assert_eq!(rows[0].1["byOrigin"], "human");
    Ok(())
}

#[tokio::test]
async fn bot_relayed_answer_keeps_bot_answer_row_in_addition_to_answered_row() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let question = ctx.interaction(&root, "question")?;

    // A Bot device, an owner on its allowlist, and the open card ticket that
    // binds this interaction to that bot (design §5.1 #1).
    let bot_token = ctx.hub.mint_bot_device_token("dd2-bot").await?;
    let bot_device = ctx.device_id("dd2-bot")?;
    let open_id = "ou_dd2_owner";
    ctx.db()?.execute(
        "INSERT INTO bot_owner_allowlist (device_id, open_id, updated_at)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![bot_device, open_id, NOW],
    )?;
    let expires_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64
        + 600_000;
    ctx.db()?.execute(
        "INSERT INTO card_tickets
            (ticket_id, device_id, interaction_id, instance_id, session_key,
             state, request_version, process_generation, request_json,
             created_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, 'feishu:dd2', 'open', 1, 1, '{}', 0, ?5)",
        rusqlite::params![
            "tkt_dd2", bot_device, question, root, expires_ms
        ],
    )?;

    let response = ctx
        .answer(
            &bot_token,
            &question,
            json!({
                "answer": { "kind": "question", "answers": {} },
                "actingOpenId": open_id,
            }),
        )
        .await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    // Oldest first: both rows are present and distinct — additive, not
    // replaced — naming the same subject.
    let rows = ctx.audits(&question)?;
    let actions: Vec<&str> = rows.iter().map(|(action, _)| action.as_str()).collect();
    assert!(actions.contains(&"interaction.answered"), "{actions:?}");
    assert!(actions.contains(&"interaction.bot-answer"), "{actions:?}");
    let answered = &rows
        .iter()
        .find(|(action, _)| action == "interaction.answered")
        .unwrap()
        .1;
    assert_eq!(answered["byOrigin"], "bot");
    assert_eq!(answered["byDevice"], json!(bot_device));
    assert_eq!(answered["answeredByLevel"], 0);
    let relay = &rows
        .iter()
        .find(|(action, _)| action == "interaction.bot-answer")
        .unwrap()
        .1;
    assert_eq!(relay["actingOpenId"], open_id);
    assert_eq!(relay["relayedBy"], "feishu");

    // The ticket was flipped to answered by the existing relay settlement.
    let state: String = ctx.db()?.query_row(
        "SELECT state FROM card_tickets WHERE ticket_id = 'tkt_dd2'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(state, "answered");
    Ok(())
}

// --- Hub→Node frame provenance ---------------------------------------------

#[tokio::test]
async fn node_rpc_frame_carries_truthful_origin_and_bound_instance() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&root)).await?;

    // Agent (parent credential) answering the child's interaction.
    let child_question = ctx.interaction(&child, "question")?;
    let token = ctx.agent_token(&root).await?;
    let response = ctx.answer_question(&token, &child_question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);
    let frame = ctx.node.frame("interaction.answer");
    assert_eq!(frame["origin"], "agent");
    assert_eq!(frame["byInstanceId"], json!(root));
    assert_eq!(frame["byDevice"], json!(ctx.device_id(&format!("agent-{root}"))?));

    // Human operator answering a separate interaction.
    let own_question = ctx.interaction(&root, "question")?;
    let response = ctx.answer_question(&ctx.human, &own_question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);
    // `frame` reads the most recent interaction.answer call.
    let calls = ctx.node.calls.lock().unwrap();
    let human_frame = calls
        .iter()
        .rev()
        .find(|(method, params)| {
            method == "interaction.answer"
                && params["interactionId"] == json!(own_question)
        })
        .map(|(_, params)| params.clone())
        .expect("human answer frame");
    assert_eq!(human_frame["origin"], "human");
    assert_eq!(human_frame["byInstanceId"], Value::Null);
    Ok(())
}

// --- the in-memory one-shot approval broker also audits --------------------

#[tokio::test]
async fn in_memory_one_shot_approval_answer_leaves_an_answered_audit_row() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let caller = ctx.instance("manual", None).await?;
    let sibling = ctx.instance("manual", None).await?;
    let token = ctx.agent_token(&caller).await?;

    // An out-of-scope sibling send mints the in-memory one-shot human
    // approval (no durable `interactions` row): 409 + interactionId.
    let response = ctx
        .http
        .post(format!(
            "{}/v1/instances/{sibling}/commands",
            ctx.base()
        ))
        .bearer_auth(&token)
        .json(&json!({
            "operation": "instance.send",
            "payload": {
                "input": {
                    "type": "prompt",
                    "mode": "new-turn",
                    "blocks": [{ "type": "text", "text": "needs a human" }],
                    "origin": "agent"
                },
                "completionScope": "native-turn"
            }
        }))
        .send()
        .await?;
    assert_eq!(response.status(), 409);
    let body = response.json::<Value>().await?;
    let approval_id = body["interactionId"].as_str().expect("approval id");

    // The human settles it through the same answer endpoint.
    let pending = ctx
        .http
        .get(format!("{}/v1/interactions", ctx.base()))
        .bearer_auth(&ctx.human)
        .send()
        .await?
        .json::<Value>()
        .await?;
    let digest = pending["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item.get("interactionId")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                == Some(approval_id)
        })
        .and_then(|item| item["interaction"]["request"]["inputDigest"].as_str())
        .or_else(|| {
            // The flat merged entity exposes request directly on some branches.
            pending["items"].as_array().unwrap().iter().find_map(|item| {
                item.pointer("/request/inputDigest").and_then(Value::as_str)
            })
        })
        .expect("pending approval digest")
        .to_string();
    let response = ctx
        .answer(
            &ctx.human,
            approval_id,
            json!({
                "answer": {
                    "kind": "approval",
                    "optionId": "allow-once",
                    "inputDigest": digest,
                }
            }),
        )
        .await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    let detail = ctx.answered_detail(approval_id)?;
    assert_eq!(detail["interactionKind"], "approval");
    assert_eq!(detail["byOrigin"], "human");
    assert_eq!(detail["byInstanceId"], Value::Null);
    // The chain starts at the agent instance the approval was gating.
    assert_eq!(chain_ids(&detail), vec![caller.clone()]);
    assert_eq!(detail["answeredByLevel"], 0);
    // No bot was involved.
    let actions: Vec<String> = ctx
        .audits(approval_id)?
        .into_iter()
        .map(|(action, _)| action)
        .collect();
    assert_eq!(actions, vec!["interaction.answered".to_string()]);
    Ok(())
}

// --- dead middle ancestor ---------------------------------------------------

#[tokio::test]
async fn chain_walk_tolerates_a_deleted_middle_ancestor() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let middle = ctx.instance("manual", Some(&root)).await?;
    let leaf = ctx.instance("manual", Some(&middle)).await?;
    let question = ctx.interaction(&leaf, "question")?;

    // The middle instance row is gone before the answer lands.
    ctx.db()?
        .execute("DELETE FROM instances WHERE id = ?1", rusqlite::params![middle])?;

    let response = ctx.answer_question(&ctx.human, &question).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);

    let detail = ctx.answered_detail(&question)?;
    // The walk reaches leaf, records its edge into the missing middle, then
    // stops — the answer still audits and the dangling edge is preserved.
    assert_eq!(chain_ids(&detail), vec![leaf.clone()]);
    assert_eq!(detail["chain"][0]["parentInstanceId"], json!(middle));
    assert_eq!(detail["answeredByLevel"], 0);
    Ok(())
}

// --- after-the-fact reconstruction across several answers on one subject ----

#[tokio::test]
async fn audit_rows_for_one_subject_reconstruct_every_hop_in_id_order() -> Result<()> {
    let _flag = flag_on().await;
    let ctx = Ctx::boot().await?;
    let root = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&root)).await?;
    let question = ctx.interaction(&child, "question")?;

    // Losing CAS (second command) would 409; here just one answer settles it,
    // and a same-command retry is idempotent at the Node — the Hub still only
    // mirrors once. Assert the durable reconstruction query directly.
    let response = ctx.answer_question(&ctx.human, &question).await?;
    assert_eq!(response.status(), 200);
    let rows = ctx.audits(&question)?;
    assert_eq!(rows.len(), 1);
    let detail = &rows[0].1;
    let hops: HashMap<String, Option<String>> = detail["chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| {
            (
                hop["instanceId"].as_str().unwrap().to_string(),
                hop["parentInstanceId"].as_str().map(str::to_string),
            )
        })
        .collect();
    assert_eq!(hops.get(&child), Some(&Some(root.clone())));
    assert_eq!(hops.get(&root), Some(&None));
    Ok(())
}
