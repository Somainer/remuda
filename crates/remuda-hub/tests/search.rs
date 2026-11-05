//! cb-search (protocol.md §8.4, main-agent.md §12.7): `GET /v1/search` over
//! HTTP, scoped exactly like the journal read.
//!
//! The scope matrix asks, for every caller and every instance, whether the
//! journal read is allowed, and requires that search returns that instance's
//! hit exactly when it is. The callers cover an Agent in each place of a
//! lineage, a foreign Agent, the owner, the owner narrowed to a seat with
//! `x-remuda-instance-id`, and a Bot device.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, RunningHub, spawn};
use serde_json::{Value, json};

const HOST: &str = "hst_search";
const PHRASE: &str = "quartermaster ledger";

struct Ctx {
    _dir: tempfile::TempDir,
    hub: RunningHub,
    http: reqwest::Client,
    human: String,
}

struct Reply {
    status: u16,
    body: Value,
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        hub.test_insert_host(HOST).await?;
        let human = hub.mint_device_token("search-owner").await?;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
        })
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        Ok(rusqlite::Connection::open(
            self._dir.path().join("hub").join("hub.sqlite"),
        )?)
    }

    /// An instance; `lineage` makes it a later chapter of that lineage and
    /// `parent` stamps the dispatch edge.
    async fn instance(&self, lineage: Option<&str>, parent: Option<&str>) -> Result<String> {
        let id = self
            .hub
            .store()
            .context("store")?
            .insert_instance(
                HOST.into(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"kind": "claude"}),
            )
            .await?
            .instance_id;
        let db = self.db()?;
        if let Some(lineage) = lineage {
            db.execute(
                "UPDATE instances SET lineage_id = ?1 WHERE id = ?2",
                rusqlite::params![lineage, id],
            )?;
        }
        if let Some(parent) = parent {
            db.execute(
                "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
                rusqlite::params![
                    json!({"kind": "claude", "parentInstanceId": parent}).to_string(),
                    id
                ],
            )?;
        }
        Ok(id)
    }

    async fn say(&self, instance: &str, node: &str, text: &str) -> Result<i64> {
        let event = json!({
            "kind": "message",
            "payload": {
                "nodeId": node, "operation": "close", "messageId": node,
                "role": "assistant", "phase": "final",
                "blocks": [{"type": "text", "text": text}],
            },
        });
        Ok(self
            .hub
            .store()
            .context("store")?
            .append_journal(HOST.into(), instance.into(), None, event)
            .await?
            .record
            .seq)
    }

    async fn get(&self, path: &str, token: &str, narrow: Option<&str>) -> Result<Reply> {
        let mut request = self
            .http
            .get(format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(instance) = narrow {
            request = request.header("x-remuda-instance-id", instance);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok(Reply {
            status,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
        })
    }

    async fn search(&self, query: &str, token: &str, narrow: Option<&str>) -> Result<Value> {
        let reply = self
            .get(&format!("/v1/search?{query}"), token, narrow)
            .await?;
        anyhow::ensure!(
            reply.status == 200,
            "search {query} -> {}: {}",
            reply.status,
            reply.body
        );
        Ok(reply.body)
    }
}

fn instances_hit(body: &Value) -> Vec<String> {
    let mut ids: Vec<String> = body["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| hit["instanceId"].as_str().unwrap_or("").to_string())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn q(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_returns_an_instance_exactly_when_its_journal_is_readable() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    // A seat lineage with two chapters, a worker the first chapter
    // dispatched, the worker's own child, and an unrelated lineage with its
    // own worker.
    let seat1 = ctx.instance(None, None).await?;
    let seat2 = ctx.instance(Some(&seat1), None).await?;
    let worker = ctx.instance(None, Some(&seat1)).await?;
    let grandchild = ctx.instance(None, Some(&worker)).await?;
    let other = ctx.instance(None, None).await?;
    let other_worker = ctx.instance(None, Some(&other)).await?;
    let all = [&seat1, &seat2, &worker, &grandchild, &other, &other_worker];
    for (n, instance) in all.iter().enumerate() {
        ctx.say(
            instance,
            &format!("m{n}"),
            &format!("the {PHRASE} entry {n}"),
        )
        .await?;
    }
    let bot = ctx.hub.mint_bot_device_token("search-bot").await?;
    let mut callers: Vec<(String, String, Option<String>)> = vec![
        ("owner".into(), ctx.human.clone(), None),
        ("bot".into(), bot, None),
        (
            "owner narrowed".into(),
            ctx.human.clone(),
            Some(seat2.clone()),
        ),
    ];
    for instance in [&seat1, &seat2, &worker, &grandchild, &other] {
        let token = ctx
            .hub
            .test_mint_agent_token(&format!("agent-{instance}"), instance)
            .await?;
        callers.push((format!("agent {instance}"), token, None));
    }
    for (name, token, narrow) in &callers {
        let found = instances_hit(
            &ctx.search(
                &format!("q={}&limit=50", q(PHRASE)),
                token,
                narrow.as_deref(),
            )
            .await?,
        );
        let mut readable = Vec::new();
        for instance in all {
            let read = ctx
                .get(
                    &format!("/v1/instances/{instance}/journal"),
                    token,
                    narrow.as_deref(),
                )
                .await?;
            if read.status == 200 {
                readable.push(instance.to_string());
            }
        }
        readable.sort();
        assert_eq!(found, readable, "{name}");
    }
    // The matrix is not vacuous: the owner reads all six, a seat chapter
    // reads its lineage and its worker, the worker its child.
    let agent_seat = &callers[3].1;
    let mut expect = vec![seat1.clone(), seat2.clone(), worker.clone()];
    expect.sort();
    assert_eq!(
        instances_hit(
            &ctx.search(&format!("q={}", q(PHRASE)), agent_seat, None)
                .await?
        ),
        expect
    );
    assert_eq!(
        instances_hit(
            &ctx.search(&format!("q={}", q(PHRASE)), &ctx.human, None)
                .await?
        )
        .len(),
        6
    );
    // The narrowed owner is that seat's Agent.
    assert_eq!(
        instances_hit(
            &ctx.search(&format!("q={}", q(PHRASE)), &ctx.human, Some(&seat2))
                .await?
        ),
        expect
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_hits_never_crowd_out_or_count_against_your_own() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let seat = ctx.instance(None, None).await?;
    let worker = ctx.instance(None, Some(&seat)).await?;
    let foreign = ctx.instance(None, None).await?;
    for n in 0..60 {
        ctx.say(&foreign, &format!("f{n}"), "rollback rollback rollback")
            .await?;
    }
    let own = ctx
        .say(
            &worker,
            "w",
            "a long report in which rollback appears once among many other words",
        )
        .await?;
    let agent = ctx.hub.test_mint_agent_token("agent-seat", &seat).await?;
    let body = ctx.search("q=rollback&limit=50", &agent, None).await?;
    assert_eq!(body["hits"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["hits"][0]["instanceId"], json!(worker));
    assert_eq!(body["hits"][0]["seq"], json!(own.to_string()));
    assert_eq!(body["hits"][0]["lineageId"], json!(worker));
    assert_eq!(body["hits"][0]["matchedBy"], json!("fts"));
    assert_eq!(body["hits"][0]["kind"], json!("message"));
    // The response carries hits and two flags: no total to leak a count.
    let mut keys: Vec<&str> = body
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort();
    assert_eq!(keys, ["hits", "indexComplete", "partial"]);
    // Narrowing to the foreign instance or lineage finds nothing, not 403.
    for narrow in [format!("instance={foreign}"), format!("lineage={foreign}")] {
        let body = ctx
            .search(&format!("q=rollback&{narrow}"), &agent, None)
            .await?;
        assert_eq!(body["hits"], json!([]), "{narrow}");
    }
    // The owner sees the limit's worth, foreign first; 50 is the cap.
    let body = ctx.search("q=rollback&limit=500", &ctx.human, None).await?;
    assert_eq!(body["hits"].as_array().map(Vec::len), Some(50));
    // The status is admitted for the Agent but carries no Hub-wide counts.
    let status = ctx.get("/v1/search/status", &agent, None).await?;
    assert_eq!(status.status, 200);
    assert!(status.body.get("docs").is_none(), "{}", status.body);
    assert!(status.body.get("journalRows").is_none(), "{}", status.body);
    let status = ctx.get("/v1/search/status", &ctx.human, None).await?;
    assert_eq!(status.status, 200);
    assert!(status.body["docs"].is_string(), "{}", status.body);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_refuses_malformed_queries_and_finds_cjk_and_literal_syntax() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let seat = ctx.instance(None, None).await?;
    let seq = ctx
        .say(
            &seat,
            "m",
            "旧迁移 0042 删掉还是保留？ AND \"quoted\" NEAR(x)",
        )
        .await?;
    let found = ctx
        .search(&format!("q={}", q("迁移")), &ctx.human, None)
        .await?;
    assert_eq!(found["hits"][0]["seq"], json!(seq.to_string()), "{found}");
    assert_eq!(found["hits"][0]["matchedBy"], json!("like"));
    for literal in ["NEAR(x)", "\"quoted", "AND"] {
        let found = ctx
            .search(&format!("q={}", q(literal)), &ctx.human, None)
            .await?;
        assert_eq!(
            found["hits"].as_array().map(Vec::len),
            Some(1),
            "{literal}: {found}"
        );
    }
    for (query, why) in [
        ("q=", "empty"),
        ("q=%20%20", "blank"),
        ("q=a+b+c+d+e+f+g+h+i", "nine terms"),
        ("q=x&since=yesterday", "since"),
    ] {
        let reply = ctx
            .get(&format!("/v1/search?{query}"), &ctx.human, None)
            .await?;
        assert_eq!(reply.status, 400, "{why}: {}", reply.body);
    }
    let long = "x".repeat(257);
    let reply = ctx
        .get(&format!("/v1/search?q={long}"), &ctx.human, None)
        .await?;
    assert_eq!(reply.status, 400);
    // A relative `since` is the CLI's job; the route takes RFC 3339, any offset.
    let body = ctx
        .search("q=0042&since=2000-01-01T08:00:00%2B08:00", &ctx.human, None)
        .await?;
    assert_eq!(body["hits"].as_array().map(Vec::len), Some(1));
    // Unauthenticated callers get nothing.
    let reply = ctx.get("/v1/search?q=0042", "not-a-token", None).await?;
    assert_eq!(reply.status, 401);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_an_instance_removes_its_hits() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let gone = ctx.instance(None, None).await?;
    ctx.say(&gone, "m", "a transient remark").await?;
    assert_eq!(
        instances_hit(&ctx.search("q=transient", &ctx.human, None).await?),
        vec![gone.clone()]
    );
    ctx.db()?.execute(
        "UPDATE instances SET lifecycle = 'exited' WHERE id = ?1",
        rusqlite::params![gone],
    )?;
    let response = ctx
        .http
        .delete(format!("http://{}/v1/instances/{gone}", ctx.hub.addr))
        .bearer_auth(&ctx.human)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "delete -> {}: {}",
        response.status(),
        response.text().await?
    );
    assert_eq!(
        ctx.search("q=transient", &ctx.human, None).await?["hits"],
        json!([])
    );
    let left: i64 = ctx.db()?.query_row(
        "SELECT COUNT(*) FROM journal_search_docs WHERE instance_id = ?1",
        rusqlite::params![gone],
        |row| row.get(0),
    )?;
    assert_eq!(left, 0);
    Ok(())
}
