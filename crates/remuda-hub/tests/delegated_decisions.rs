//! D-051 (c-deleg1): delegated-decision routing.
//!
//! The two handler relaxations (`GET /v1/interactions`,
//! `POST /v1/interactions/{id}/answer`) for the one-hop `owns()` edge; the
//! three mechanical exclusions (approval kind, bypass-posture child,
//! bypass-posture parent); the in-memory approval broker origin gate; the
//! literal middleware predicates; and the feature-off behavior being
//! byte-for-byte the old operator-only refusal.
//!
//! All fixtures are scripted directly against the Hub + its SQLite file (no
//! real Node): pending rows are plain `interactions` inserts, and the one
//! successful-answer case mounts a scripted Node transport.

use anyhow::{Context, Result};
use remuda_hub::store_test_support::Store;
use remuda_hub::{HubConfig, HubError, NodeTransport, TransportKind, spawn};
use serde_json::{Value, json};
use std::sync::LazyLock;
use tempfile::TempDir;

const NOW: &str = "2026-09-22T00:00:00.000Z";

// The D-051 switch override is process-global. Every test that depends on its
// value holds this lock for its whole life, so on/off cases in this file never
// overlap. Other test files only hit `/v1/interactions` as operators, whose
// branch is flag-independent.
static FLAG_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::const_new(()));

struct FlagGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

async fn flag(value: Option<bool>) -> FlagGuard {
    let guard = FLAG_LOCK.lock().await;
    remuda_hub::delegated_decisions_test_support::set_global_override(value);
    FlagGuard { _guard: guard }
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        remuda_hub::delegated_decisions_test_support::set_global_override(None);
    }
}

struct Ctx {
    _dir: TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    store: Store,
    host: String,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("data"))).await?;
        let human = hub.mint_device_token("dd-human").await?;
        let host = remuda_protocol::HostId::new().as_id().as_str().to_string();
        hub.test_insert_host(&host).await?;
        let store = hub.store().context("hub store")?.clone();
        Ok(Self {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            store,
            host,
        })
    }

    fn base(&self) -> String {
        format!("http://{}", self.hub.addr)
    }

    /// Insert an instance directly, then (for children) stamp the parent edge
    /// into its spec via SQL — bypassing the API-layer dispatch-grant check,
    /// which is irrelevant to the `owns()` predicate under test.
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
            let db = rusqlite::Connection::open(self._dir.path().join("data").join("hub.sqlite"))?;
            db.execute(
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

    /// Insert a pending interaction row directly. The id is a canonical
    /// UUIDv7 brand so the HTTP layer parses it as an `InteractionId`.
    fn interaction(&self, instance_id: &str, kind: &str) -> Result<String> {
        let id = remuda_protocol::InteractionId::new()
            .as_id()
            .as_str()
            .to_string();
        let db = rusqlite::Connection::open(self._dir.path().join("data").join("hub.sqlite"))?;
        db.execute(
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

    async fn list(&self, token: &str, query: &[(&str, &str)]) -> Result<reqwest::Response> {
        Ok(self
            .http
            .get(format!("{}/v1/interactions", self.base()))
            .query(query)
            .bearer_auth(token)
            .send()
            .await?)
    }

    async fn answer(
        &self,
        token: &str,
        interaction_id: &str,
        kind: &str,
    ) -> Result<reqwest::Response> {
        // Shape differs only for approvals; the digest is well-formed wire
        // shape but irrelevant, since the approval-kind gate refuses before
        // any digest comparison.
        let answer = if kind == "approval" {
            json!({
                "kind": "approval",
                "optionId": "allow-once",
                "inputDigest": format!("sha256:{}", "0".repeat(64))
            })
        } else {
            json!({ "kind": kind, "answers": {} })
        };
        Ok(self
            .http
            .post(format!(
                "{}/v1/interactions/{interaction_id}/answer",
                self.base()
            ))
            .bearer_auth(token)
            .json(&json!({ "answer": answer }))
            .send()
            .await?)
    }

    async fn raw(&self, method: &str, path: &str, token: &str) -> Result<reqwest::Response> {
        let request = self
            .http
            .request(method.parse()?, format!("{}{path}", self.base()))
            .bearer_auth(token);
        Ok(request.send().await?)
    }

    /// POST with a well-formed answer body, for probes that must clear body
    /// extraction and reach path/id handling.
    async fn raw_answer_post(&self, path: &str, token: &str) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}{path}", self.base()))
            .bearer_auth(token)
            .json(&json!({ "answer": { "kind": "question", "answers": {} } }))
            .send()
            .await?)
    }

    fn interaction_state(&self, interaction_id: &str) -> Result<String> {
        let db = rusqlite::Connection::open(self._dir.path().join("data").join("hub.sqlite"))?;
        Ok(db.query_row(
            "SELECT state FROM interactions WHERE id = ?1",
            rusqlite::params![interaction_id],
            |row| row.get(0),
        )?)
    }
}

fn item_ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["interactionId"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// --- owns() one-hop edge: list visibility ---------------------------------

#[tokio::test]
async fn agent_lists_own_and_direct_child_pending_but_not_sibling_or_excluded() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let sibling = ctx.instance("manual", None).await?;
    let bypass_child = ctx.instance("bypassPermissions", Some(&parent)).await?;

    let own_question = ctx.interaction(&parent, "question")?;
    let child_question = ctx.interaction(&child, "question")?;
    let child_elicitation = ctx.interaction(&child, "elicitation")?;
    let child_approval = ctx.interaction(&child, "approval")?;
    let sibling_question = ctx.interaction(&sibling, "question")?;
    let bypass_question = ctx.interaction(&bypass_child, "question")?;

    let token = ctx.agent_token(&parent).await?;

    // Unpinned page: self + owned non-approval, non-bypass rows only. The
    // approval comes from the SQL branch here; the same filter covers the
    // in-memory agent_approvals branch merged into the same page.
    let response = ctx.list(&token, &[]).await?;
    assert_eq!(response.status(), 200);
    let body = response.json::<Value>().await?;
    let mut ids = item_ids(&body);
    ids.sort();
    let mut expected = [
        own_question.clone(),
        child_question.clone(),
        child_elicitation.clone(),
    ];
    expected.sort();
    assert_eq!(ids, expected, "unpinned page: {body}");
    assert!(!ids.contains(&child_approval));
    assert!(!ids.contains(&sibling_question));
    assert!(!ids.contains(&bypass_question));

    // Pinned to the child: question + elicitation survive, approval does not.
    let response = ctx.list(&token, &[("instanceId", child.as_str())]).await?;
    assert_eq!(response.status(), 200);
    let body = response.json::<Value>().await?;
    let ids = item_ids(&body);
    assert!(ids.contains(&child_question));
    assert!(ids.contains(&child_elicitation));
    assert!(!ids.contains(&child_approval));

    // Pinned to self keeps working.
    let response = ctx.list(&token, &[("instanceId", parent.as_str())]).await?;
    assert_eq!(response.status(), 200);
    assert!(item_ids(&response.json::<Value>().await?).contains(&own_question));

    // Pinned to an instance the caller does not own is a flat refusal.
    let response = ctx
        .list(&token, &[("instanceId", sibling.as_str())])
        .await?;
    assert_eq!(response.status(), 403);

    // Approval-kind query on an owned child yields nothing: approvals never
    // route, regardless of how the query is phrased.
    let response = ctx
        .list(
            &token,
            &[("instanceId", child.as_str()), ("kind", "approval")],
        )
        .await?;
    assert_eq!(response.status(), 200);
    assert!(item_ids(&response.json::<Value>().await?).is_empty());
    Ok(())
}

// --- owns() edge + Node first-answer-wins dispatch ------------------------

#[tokio::test]
async fn agent_answer_to_owned_and_self_interaction_reaches_node_cas() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let child_question = ctx.interaction(&child, "question")?;
    let own_question = ctx.interaction(&parent, "question")?;
    // The Hub forwards to the owning host; the scripted Node accepts the CAS.
    ctx.hub
        .test_set_node_reply(
            &ctx.host,
            Some(json!({ "jsonrpc": "2.0", "id": "1", "result": { "state": "answered" } })),
        )
        .await;
    let token = ctx.agent_token(&parent).await?;

    let response = ctx.answer(&token, &child_question, "question").await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);
    assert_eq!(ctx.interaction_state(&child_question)?, "answer-committed");

    let response = ctx.answer(&token, &own_question, "question").await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);
    assert_eq!(ctx.interaction_state(&own_question)?, "answer-committed");
    Ok(())
}

#[tokio::test]
async fn agent_answer_to_unowned_interaction_is_forbidden() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let sibling = ctx.instance("manual", None).await?;
    let question = ctx.interaction(&sibling, "question")?;
    // No live Node: had the gate wrongly admitted the answer, it would 404.
    let token = ctx.agent_token(&parent).await?;
    let response = ctx.answer(&token, &question, "question").await?;
    assert_eq!(response.status(), 403);
    assert_eq!(ctx.interaction_state(&question)?, "pending");
    Ok(())
}

// --- exclusion (a): approval kind never routes ----------------------------

#[tokio::test]
async fn agent_answer_to_approval_kind_is_forbidden_even_when_owned() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let child_approval = ctx.interaction(&child, "approval")?;
    let own_approval = ctx.interaction(&parent, "approval")?;
    let token = ctx.agent_token(&parent).await?;

    let response = ctx.answer(&token, &child_approval, "approval").await?;
    assert_eq!(response.status(), 403);
    let response = ctx.answer(&token, &own_approval, "approval").await?;
    assert_eq!(response.status(), 403);
    assert_eq!(ctx.interaction_state(&child_approval)?, "pending");
    assert_eq!(ctx.interaction_state(&own_approval)?, "pending");
    Ok(())
}

// --- exclusion (b): bypass posture on either side -------------------------

#[tokio::test]
async fn bypass_posture_child_interaction_is_refused_and_hidden() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("bypassPermissions", Some(&parent)).await?;
    let question = ctx.interaction(&child, "question")?;
    let token = ctx.agent_token(&parent).await?;

    let response = ctx.answer(&token, &question, "question").await?;
    assert_eq!(response.status(), 403);
    assert_eq!(ctx.interaction_state(&question)?, "pending");

    let response = ctx.list(&token, &[("instanceId", child.as_str())]).await?;
    assert_eq!(response.status(), 200);
    assert!(item_ids(&response.json::<Value>().await?).is_empty());
    Ok(())
}

#[tokio::test]
async fn bypass_posture_parent_cannot_answer_or_see_child_pending() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    // A Human session can run bypassPermissions; its instance-bound credential
    // is Agent origin. That mandate must not be lent to a child's answer.
    let parent = ctx.instance("bypassPermissions", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let question = ctx.interaction(&child, "question")?;
    let token = ctx.agent_token(&parent).await?;

    let response = ctx.answer(&token, &question, "question").await?;
    assert_eq!(response.status(), 403);
    assert_eq!(ctx.interaction_state(&question)?, "pending");

    // The parent-posture exclusion filters every routed row, even manual ones.
    let response = ctx.list(&token, &[("instanceId", child.as_str())]).await?;
    assert_eq!(response.status(), 200);
    assert!(item_ids(&response.json::<Value>().await?).is_empty());
    Ok(())
}

// --- feature off: byte-for-byte the old operator-only behavior ------------

#[tokio::test]
async fn agent_interactions_are_operator_only_when_switch_is_off() -> Result<()> {
    let _flag = flag(Some(false)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let question = ctx.interaction(&child, "question")?;
    let token = ctx.agent_token(&parent).await?;

    assert_eq!(ctx.list(&token, &[]).await?.status(), 403);
    assert_eq!(
        ctx.list(&token, &[("instanceId", child.as_str())])
            .await?
            .status(),
        403
    );
    // Self-list was operator-only before D-051 too.
    assert_eq!(
        ctx.list(&token, &[("instanceId", parent.as_str())])
            .await?
            .status(),
        403
    );
    assert_eq!(
        ctx.answer(&token, &question, "question").await?.status(),
        403
    );
    assert_eq!(ctx.interaction_state(&question)?, "pending");
    Ok(())
}

// --- middleware vs handler layering ---------------------------------------

#[tokio::test]
async fn middleware_admits_only_the_literal_interaction_predicates() -> Result<()> {
    let _flag = flag(Some(false)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let question = ctx.interaction(&child, "question")?;
    let agent = ctx.agent_token(&parent).await?;

    // Whitelist MISS: the Agent is refused at the middleware, before routing.
    // The SPA fallback serves the same unknown GET to an operator (200), so
    // for the agent the 403 can only have come from the middleware — no
    // interaction handler exists at that path.
    assert_eq!(
        ctx.raw("GET", "/v1/interactions/int_x", &agent)
            .await?
            .status(),
        403
    );
    assert_eq!(
        ctx.raw("GET", "/v1/interactions/int_x", &ctx.human)
            .await?
            .status(),
        200
    );
    assert_eq!(
        ctx.raw("POST", "/v1/interactions", &agent).await?.status(),
        403
    );
    assert_eq!(
        ctx.raw("POST", "/v1/interactions/int_x/answer-extra", &agent)
            .await?
            .status(),
        403
    );

    // Whitelist HIT with a malformed id: the middleware lets the request
    // through and execution reaches the handler, which rejects the id shape
    // with 400. A middleware refusal is always 403, so 400 proves the exact
    // `…/answer` literal predicate admitted the request.
    assert_eq!(
        ctx.raw_answer_post("/v1/interactions/not-an-id/answer", &agent)
            .await?
            .status(),
        400
    );

    // Whitelist hit with the switch off and a valid id: past the middleware,
    // the handler returns 403 — the malformed-id probe above pins the layer.
    assert_eq!(ctx.list(&agent, &[]).await?.status(), 403);
    assert_eq!(
        ctx.answer(&agent, &question, "question").await?.status(),
        403
    );
    Ok(())
}

// --- exclusion (a): the in-memory grant branch merges before filtering ----

#[tokio::test]
async fn in_memory_approval_grant_is_filtered_from_agent_page_after_merge() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let sibling = ctx.instance("manual", None).await?;
    let token = ctx.agent_token(&parent).await?;

    // An out-of-scope sibling send makes the Hub mint its one-shot human
    // approval in the IN-MEMORY agent_approvals broker (not the SQL table).
    let response = ctx
        .http
        .post(format!("{}/v1/instances/{sibling}/commands", ctx.base()))
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
    assert_eq!(response.status(), 409, "{}", response.text().await?);

    // The merged Agent page (SQL + in-memory grants + live RPC) must not carry
    // the approval, either unpinned or pinned to the caller's own instance.
    for query in [Vec::new(), vec![("instanceId", parent.as_str())]] {
        let body = ctx.list(&token, &query).await?.json::<Value>().await?;
        let items = body["items"].as_array().cloned().unwrap_or_default();
        assert!(
            items
                .iter()
                .all(|item| item["kind"].as_str() != Some("approval")),
            "approval grant leaked into agent page: {body}"
        );
    }

    // The operator page still presents the in-memory grant.
    let body = ctx.list(&ctx.human, &[]).await?.json::<Value>().await?;
    let kinds: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"approval"), "operator page: {body}");
    Ok(())
}

// --- operators still see the unfiltered page ------------------------------

#[tokio::test]
async fn operators_still_see_and_pin_every_pending_with_switch_on() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let sibling = ctx.instance("manual", None).await?;
    let bypass_child = ctx.instance("bypassPermissions", Some(&parent)).await?;
    let child_question = ctx.interaction(&child, "question")?;
    let child_approval = ctx.interaction(&child, "approval")?;
    let sibling_question = ctx.interaction(&sibling, "question")?;
    let bypass_question = ctx.interaction(&bypass_child, "question")?;
    let _ = parent;

    let response = ctx.list(&ctx.human, &[]).await?;
    assert_eq!(response.status(), 200);
    let ids = item_ids(&response.json::<Value>().await?);
    // Approvals, siblings, and bypass-posture rows all stay operator-visible.
    for id in [
        &child_question,
        &child_approval,
        &sibling_question,
        &bypass_question,
    ] {
        assert!(ids.contains(id), "operator page missing {id}");
    }
    Ok(())
}

// --- D-051 (6d): plan-review self-exclusion and the real Node CAS ---------
//
// A plan-review gates the target instance's OWN plan, so it does NOT accept
// the self edge in `owns()`: the child cannot list/answer it; its direct
// parent (or a human) does. The answer rides the Node first-answer-wins CAS.
//
// These tests do NOT script the Node result: the test transport is backed by
// the real remuda-driver InteractionBroker with a counting owner, so the
// Hub->Node interaction.answer path actually wins/loses the CAS exactly once.

use remuda_driver::interaction::{
    AnswerCaller, BrokerConfig, BrokerError, InteractionBroker, InteractionOwner, NativeRequestId,
    PendingSpec,
};
use remuda_protocol::{
    CommandId, DecisionEffect, DecisionOption, Digest, Id as PId, InteractionAnswer,
    InteractionRequest, PlanReviewRequest, U64,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const PLAN_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Owner that counts how many times the winning answer was actually applied,
/// so tests can assert exactly one driver delivery under a race.
struct CountingOwner {
    delivered: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl InteractionOwner for CountingOwner {
    async fn apply_answer(
        &self,
        _id: remuda_protocol::InteractionId,
        _answer: InteractionAnswer,
    ) -> Result<(), BrokerError> {
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn deny_or_cancel(&self, _id: remuda_protocol::InteractionId) -> Result<(), BrokerError> {
        Ok(())
    }
}

/// A Node transport backed by the real driver interaction broker.
struct BrokerTransport {
    broker: Arc<InteractionBroker>,
}

fn rpc_frame(result: Result<remuda_driver::interaction::AnswerOutcome, BrokerError>) -> Value {
    match result {
        Ok(_) => json!({
            "jsonrpc": "2.0", "id": "1",
            "result": { "outcome": "accepted", "state": "answer-committed" }
        }),
        Err(BrokerError::Superseded { .. }) => json!({
            "jsonrpc": "2.0", "id": "1",
            "error": { "code": -32004, "message": "interaction already answered" }
        }),
        Err(BrokerError::Expired) => json!({
            "jsonrpc": "2.0", "id": "1",
            "error": { "code": -32005, "message": "interaction expired" }
        }),
        Err(other) => json!({
            "jsonrpc": "2.0", "id": "1",
            "error": { "code": -32602, "message": other.to_string() }
        }),
    }
}

impl NodeTransport for BrokerTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::OutboundWss
    }
    fn call(
        &self,
        method: &str,
        params: Value,
        _timeout: std::time::Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, HubError>> + Send + '_>> {
        // The merged inbox also polls the Node live list; these tests rely on
        // the durable Hub row only, so the live side is empty.
        if method == "interaction.list" {
            return Box::pin(std::future::ready(Ok(Some(json!({ "items": [] })))));
        }
        assert_eq!(
            method, "interaction.answer",
            "unexpected Node method {method}"
        );
        let broker = self.broker.clone();
        Box::pin(async move {
            let interaction_id = params["interactionId"]
                .as_str()
                .and_then(|s| remuda_protocol::InteractionId::try_from(s.to_string()).ok())
                .expect("interactionId");
            let answer: InteractionAnswer =
                serde_json::from_value(params["answer"].clone()).expect("answer");
            let command_id: CommandId =
                serde_json::from_value(params["commandId"].clone()).expect("commandId");
            let device_id: PId =
                serde_json::from_value(params["byDevice"].clone()).expect("byDevice");
            let origin = match params["origin"].as_str() {
                Some("human") => remuda_driver::LaunchOrigin::Human,
                Some("agent") => remuda_driver::LaunchOrigin::Agent,
                other => panic!("unexpected origin {other:?}"),
            };
            let instance_id = params
                .get("byInstanceId")
                .and_then(Value::as_str)
                .and_then(|s| remuda_protocol::InstanceId::try_from(s.to_string()).ok());
            let caller = AnswerCaller {
                device_id,
                origin,
                instance_id,
            };
            let result = broker
                .answer_for(interaction_id, answer, caller, command_id)
                .await;
            Ok(Some(rpc_frame(result)))
        })
    }
    fn notify(
        &self,
        _method: &str,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<bool, HubError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

impl Ctx {
    /// Insert a pending interaction row with a caller-chosen id (matches the
    /// broker ticket id).
    fn interaction_with_id(&self, instance_id: &str, kind: &str, id: &str) -> Result<()> {
        let db = rusqlite::Connection::open(self._dir.path().join("data").join("hub.sqlite"))?;
        db.execute(
            "INSERT INTO interactions
                (id, instance_id, host_id, kind, state, blocking, payload_json,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 1, '{}', ?5, ?5)",
            rusqlite::params![id, instance_id, self.host, kind, NOW],
        )?;
        Ok(())
    }

    /// POST a well-formed plan-review answer.
    async fn plan_answer(&self, token: &str, interaction_id: &str) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!(
                "{}/v1/interactions/{interaction_id}/answer",
                self.base()
            ))
            .bearer_auth(token)
            .json(&json!({
                "answer": {
                    "kind": "plan-review",
                    "optionId": "approve",
                    "planRevision": "1",
                    "planDigest": PLAN_DIGEST,
                    "feedback": null
                }
            }))
            .send()
            .await?)
    }

    /// Back the owning host's transport with a real driver broker holding one
    /// pending plan-review for `child`, and create the matching Hub row.
    /// Returns the interaction id and the delivery counter.
    async fn seed_plan_review_node(&self, child: &str) -> Result<(String, Arc<AtomicUsize>)> {
        let (broker, mut observations) =
            InteractionBroker::new(BrokerConfig::default()).expect("broker");
        // Drain answered observations so the broker channel never blocks.
        tokio::spawn(async move { while observations.recv().await.is_some() {} });
        let delivered = Arc::new(AtomicUsize::new(0));
        broker
            .register_owner(
                child.parse::<remuda_protocol::InstanceId>()?,
                Arc::new(CountingOwner {
                    delivered: delivered.clone(),
                }),
            )
            .await;
        let review = PlanReviewRequest {
            title: "Plan review".into(),
            plan_ref: PId::new("obj")?,
            plan_revision: U64(1),
            plan_digest: Digest::try_from(PLAN_DIGEST.to_string())?,
            options: vec![
                DecisionOption {
                    id: "approve".into(),
                    label: "Approve".into(),
                    effect: DecisionEffect::AllowOnce,
                    native_value_ref: PId::new("obj")?,
                },
                DecisionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    effect: DecisionEffect::Deny,
                    native_value_ref: PId::new("obj")?,
                },
            ],
            allow_feedback: true,
            plan: Some("# the plan".into()),
        };
        let id = broker
            .insert(PendingSpec {
                instance_id: child.parse()?,
                host_id: remuda_protocol::HostId::new(),
                native_request_id: NativeRequestId {
                    request_id: "perm-plan".into(),
                    tool_use_id: None,
                },
                payload: InteractionRequest::PlanReview(Box::new(review)),
                run_generation: U64(1),
                tool_name: Some("ExitPlanMode".into()),
            })
            .await
            .expect("insert ticket");
        let id = id.as_id().as_str().to_string();
        self.interaction_with_id(child, "plan-review", &id)?;
        self.hub
            .test_set_node_transport(
                &self.host,
                Arc::new(BrokerTransport { broker }) as Arc<dyn NodeTransport>,
            )
            .await;
        Ok((id, delivered))
    }
}

#[tokio::test]
async fn a_parents_plan_review_answer_wins_the_real_node_cas_once() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let (review, delivered) = ctx.seed_plan_review_node(&child).await?;
    let token = ctx.agent_token(&parent).await?;

    let response = ctx.plan_answer(&token, &review).await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);
    assert_eq!(ctx.interaction_state(&review)?, "answer-committed");
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        1,
        "the winning answer must be applied to the driver exactly once"
    );
    Ok(())
}

#[tokio::test]
async fn the_child_cannot_answer_or_even_see_its_own_plan_review() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = Ctx::boot().await?;
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let (review, delivered) = ctx.seed_plan_review_node(&child).await?;
    let child_token = ctx.agent_token(&child).await?;
    let parent_token = ctx.agent_token(&parent).await?;

    // The child is refused before the Node is ever contacted.
    let response = ctx.plan_answer(&child_token, &review).await?;
    assert_eq!(response.status(), 403);
    assert_eq!(ctx.interaction_state(&review)?, "pending");
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        0,
        "a 403 must not reach the CAS"
    );

    // And it does not appear in the child's own merged inbox.
    let body = ctx.list(&child_token, &[]).await?.json::<Value>().await?;
    assert!(
        !item_ids(&body).contains(&review),
        "the child must not see its own plan review"
    );
    // The direct parent does see it.
    let body = ctx.list(&parent_token, &[]).await?.json::<Value>().await?;
    assert!(
        item_ids(&body).contains(&review),
        "the parent must see the child's plan review"
    );
    Ok(())
}

#[tokio::test]
async fn when_human_and_parent_answer_concurrently_one_wins_and_one_is_superseded() -> Result<()> {
    let _flag = flag(Some(true)).await;
    let ctx = std::sync::Arc::new(Ctx::boot().await?);
    let parent = ctx.instance("manual", None).await?;
    let child = ctx.instance("manual", Some(&parent)).await?;
    let (review, delivered) = ctx.seed_plan_review_node(&child).await?;
    let parent_token = ctx.agent_token(&parent).await?;
    let human_token = ctx.human.clone();

    // Fire both answers concurrently; the broker mutex decides the winner.
    let review_for_parent = review.clone();
    let review_for_human = review.clone();
    let parent_answer = {
        let ctx = std::sync::Arc::clone(&ctx);
        async move {
            ctx.plan_answer(&parent_token, &review_for_parent)
                .await
                .expect("parent request")
                .status()
                .as_u16()
        }
    };
    let human_answer = {
        let ctx = std::sync::Arc::clone(&ctx);
        async move {
            ctx.plan_answer(&human_token, &review_for_human)
                .await
                .expect("human request")
                .status()
                .as_u16()
        }
    };
    let (parent_status, human_status) = tokio::join!(parent_answer, human_answer);

    let statuses = [parent_status, human_status];
    assert!(
        statuses.contains(&200) && statuses.contains(&409),
        "expected one accepted (200) and one Superseded (409), got {statuses:?}"
    );
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        1,
        "exactly one answer must reach the driver under a concurrent race"
    );
    assert_eq!(ctx.interaction_state(&review)?, "answer-committed");
    Ok(())
}
