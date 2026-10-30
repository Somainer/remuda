//! D-060 (cb-memory-core): the main agent's memory over HTTP.
//!
//! Every case drives the in-process Hub with Human, Agent and Bot device
//! tokens. Seats are store-inserted continuity lineages (a restart policy, no
//! grants), so no fake Node is needed; a continuation is the store's own
//! fence transaction. The backup case runs the real `scripts/hub-backup.sh`
//! with a byte-copying stand-in for `age` (encryption is not under test here;
//! the member set and the restore are).

use anyhow::{Context, Result};
use remuda_hub::store_test_support::{ContinuationResumeRequest, ContinuationResumeResult};
use remuda_hub::{HubConfig, RunningHub, spawn};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const HOST: &str = "hst_memory";

struct Ctx {
    dir: tempfile::TempDir,
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
        Self::spawn_in(dir, "hub").await
    }

    async fn spawn_in(dir: tempfile::TempDir, data: &str) -> Result<Ctx> {
        let hub = spawn(HubConfig::for_test(dir.path().join(data))).await?;
        hub.test_insert_host(HOST).await?;
        let human = hub.mint_device_token("memory-owner").await?;
        Ok(Ctx {
            dir,
            hub,
            http: reqwest::Client::new(),
            human,
        })
    }

    /// A continuity lineage whose first chapter is current; returns its id.
    async fn seat(&self) -> Result<String> {
        let record = self
            .hub
            .store()
            .context("store")?
            .insert_instance_delegated(
                HOST.into(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"origin": "human"}),
                remuda_hub::store_test_support::continuity_delegation(),
                // Hub-internal seeding work, as main's own store-insert tests
                // use: no Agent initiator, never fenced.
                remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
            )
            .await?;
        Ok(record.instance_id)
    }

    /// Continue the lineage into its next chapter (fences the current one).
    async fn continue_lineage(&self, current: &str, generation: i64) -> Result<String> {
        let outcome = self
            .hub
            .store()
            .context("store")?
            .continuation_resume(ContinuationResumeRequest {
                addressed_instance_id: current.into(),
                expected_generation: generation,
                host_id: HOST.into(),
                spec: json!({"kind": "claude", "driver": "claude-sdk"}),
                operation: "instance.create".into(),
                prompt: None,
                origin: "human".into(),
                title: None,
                driver: "claude-sdk".into(),
            })
            .await?;
        match outcome {
            ContinuationResumeResult::Resumed(resumed) => Ok(resumed.successor.instance_id),
            other => anyhow::bail!("expected a successor, got {other:?}"),
        }
    }

    async fn agent(&self, instance: &str) -> Result<String> {
        self.hub
            .test_mint_agent_token(&format!("agent-{instance}"), instance)
            .await
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        narrow_to: Option<&str>,
        body: Option<Value>,
    ) -> Result<Reply> {
        let mut request = self
            .http
            .request(method.parse()?, format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(instance) = narrow_to {
            request = request.header("x-remuda-instance-id", instance);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(Reply { status, body })
    }

    async fn ok(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let reply = self.call(method, path, token, None, body).await?;
        anyhow::ensure!(
            reply.status == 200,
            "{method} {path} → {}: {}",
            reply.status,
            reply.body
        );
        Ok(reply.body)
    }
}

fn entry(slug: &str, kind: &str, body: &str) -> Value {
    json!({
        "slug": slug,
        "type": kind,
        "title": format!("Title {slug}"),
        "description": format!("About {slug}"),
        "body": body,
    })
}

fn mem(lineage: &str, rest: &str) -> String {
    format!("/v1/lineages/{lineage}/memory{rest}")
}

#[tokio::test]
async fn concurrent_writes_at_the_same_revision_get_one_200_and_one_409() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    for round in 0..5 {
        let slug = format!("pref-{round}");
        // Two creates of one slug: one wins, one is MEMORY_SLUG_EXISTS.
        let body = entry(&slug, "user", "v1");
        let collection = mem(&lineage, "");
        let (a, b) = tokio::join!(
            ctx.call("POST", &collection, &seat, None, Some(body.clone())),
            ctx.call("POST", &collection, &ctx.human, None, Some(body)),
        );
        let (a, b) = (a?, b?);
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 409], "creates: {} / {}", a.body, b.body);
        let loser = if a.status == 409 { &a.body } else { &b.body };
        assert_eq!(loser["code"], "MEMORY_SLUG_EXISTS");
        assert_eq!(loser["currentRevision"], 1);

        // Two updates at revision 1: exactly one commits revision 2.
        let path = mem(&lineage, &format!("/entries/{slug}"));
        let (a, b) = tokio::join!(
            ctx.call(
                "PUT",
                &path,
                &seat,
                None,
                Some(json!({"expectedRevision": 1, "body": "from the seat"}))
            ),
            ctx.call(
                "PUT",
                &path,
                &ctx.human,
                None,
                Some(json!({"expectedRevision": 1, "body": "from the owner"}))
            ),
        );
        let (a, b) = (a?, b?);
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 409], "updates: {} / {}", a.body, b.body);
        let (winner, loser) = if a.status == 200 { (&a, &b) } else { (&b, &a) };
        assert_eq!(winner.body["entry"]["revision"], 2);
        assert_eq!(loser.body["code"], "MEMORY_REVISION_CONFLICT");
        assert_eq!(loser.body["currentRevision"], 2);
        let stored = ctx.ok("GET", &path, &ctx.human, None).await?;
        assert_eq!(stored["revision"], 2);
        assert_eq!(stored["body"], winner.body["entry"]["body"]);
    }

    // Update and delete must carry expectedRevision.
    let missing = ctx
        .call(
            "PUT",
            &mem(&lineage, "/entries/pref-0"),
            &seat,
            None,
            Some(json!({"body": "x"})),
        )
        .await?;
    assert_eq!(missing.status, 400, "{}", missing.body);
    let missing = ctx
        .call(
            "DELETE",
            &mem(&lineage, "/entries/pref-0"),
            &seat,
            None,
            None,
        )
        .await?;
    assert_eq!(missing.status, 400, "{}", missing.body);
    let stale = ctx
        .call(
            "DELETE",
            &mem(&lineage, "/entries/pref-0?expectedRevision=1"),
            &seat,
            None,
            None,
        )
        .await?;
    assert_eq!(stale.status, 409, "{}", stale.body);
    let deleted = ctx
        .ok(
            "DELETE",
            &mem(&lineage, "/entries/pref-0?expectedRevision=2"),
            &seat,
            None,
        )
        .await?;
    assert_eq!(deleted["deleted"]["revision"], 2);
    Ok(())
}

#[tokio::test]
async fn writes_record_origin_and_report_sizes_and_the_index() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    let mut first = entry("owner-tone", "feedback", "Why: x. How to apply: y.");
    first["sourceRefs"] = json!([{"instanceId": lineage, "seq": 3}]);
    let written = ctx
        .ok("POST", &mem(&lineage, ""), &seat, Some(first))
        .await?;
    assert_eq!(written["entry"]["origin"], "agent");
    assert_eq!(written["entry"]["writerInstance"], json!(lineage));
    assert_eq!(written["entry"]["sourceRefs"][0]["seq"], 3);
    assert_eq!(written["warnings"], json!([]));

    let owner = ctx
        .ok(
            "PUT",
            &mem(&lineage, "/entries/owner-tone"),
            &ctx.human,
            Some(json!({"expectedRevision": 1, "title": "Owner tone"})),
        )
        .await?;
    assert_eq!(owner["entry"]["origin"], "human");
    assert!(owner["entry"].get("writerInstance").is_none());
    assert_eq!(owner["entry"]["revision"], 2);

    // Soft limit: accepted, warned with the actual number.
    let body = "w".repeat(3000);
    let big = ctx
        .ok(
            "POST",
            &mem(&lineage, ""),
            &seat,
            Some(entry("big", "reference", &body)),
        )
        .await?;
    let bytes = 3000 + "Title big".len() + "About big".len();
    assert_eq!(big["entryBytes"], bytes);
    assert_eq!(
        big["warnings"],
        json!([{"limit": "entry-soft", "bytes": bytes, "maxBytes": 2048}])
    );
    // Hard limit: refused with the actual number, nothing written.
    let huge = ctx
        .call(
            "POST",
            &mem(&lineage, ""),
            &seat,
            None,
            Some(entry("huge", "reference", &"h".repeat(8192))),
        )
        .await?;
    assert_eq!(huge.status, 413, "{}", huge.body);
    assert_eq!(huge.body["code"], "MEMORY_ENTRY_TOO_LARGE");
    assert_eq!(
        huge.body["bytes"],
        8192 + "Title huge".len() + "About huge".len()
    );
    assert_eq!(huge.body["maxBytes"], 8192);
    let absent = ctx
        .call("GET", &mem(&lineage, "/entries/huge"), &seat, None, None)
        .await?;
    assert_eq!(absent.status, 404);

    let index = ctx.ok("GET", &mem(&lineage, "/index"), &seat, None).await?;
    assert_eq!(
        index["text"],
        "- [feedback] owner-tone: Owner tone — About owner-tone\n\
         - [reference] big: Title big — About big\n"
    );
    assert_eq!(index["count"], 2);
    assert_eq!(index["bytes"], index["text"].as_str().unwrap().len());

    // Search: a two-character CJK query hits by LIKE; a word by FTS.
    ctx.ok(
        "POST",
        &mem(&lineage, ""),
        &seat,
        Some(entry("scope", "project", "动手前先确认边界条件。")),
    )
    .await?;
    let hits = ctx
        .ok(
            "GET",
            &mem(&lineage, "/search?q=%E8%BE%B9%E7%95%8C"),
            &seat,
            None,
        )
        .await?;
    assert_eq!(hits["items"].as_array().unwrap().len(), 1, "{hits}");
    assert_eq!(hits["items"][0]["entry"]["slug"], "scope");
    assert_eq!(hits["items"][0]["matchedBy"], "like");
    let hits = ctx
        .ok("GET", &mem(&lineage, "/search?q=apply"), &seat, None)
        .await?;
    assert_eq!(hits["items"][0]["entry"]["slug"], "owner-tone");
    assert_eq!(hits["items"][0]["matchedBy"], "fts");
    Ok(())
}

/// r2 item 5: every term must match, so a query with more terms than the
/// search evaluates is refused, never silently shortened.
#[tokio::test]
async fn search_refuses_more_terms_than_it_evaluates() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    ctx.ok(
        "POST",
        &mem(&lineage, ""),
        &ctx.human,
        Some(entry(
            "counting",
            "reference",
            "one two three four five six seven eight",
        )),
    )
    .await?;
    let eight = ctx
        .ok(
            "GET",
            &mem(
                &lineage,
                "/search?q=one+two+three+four+five+six+seven+eight",
            ),
            &ctx.human,
            None,
        )
        .await?;
    assert_eq!(eight["items"][0]["entry"]["slug"], "counting");
    let nine = ctx
        .call(
            "GET",
            &mem(
                &lineage,
                "/search?q=one+two+three+four+five+six+seven+eight+absent",
            ),
            &ctx.human,
            None,
            None,
        )
        .await?;
    assert_eq!(
        nine.status, 400,
        "a ninth term must not be dropped: {}",
        nine.body
    );
    assert_eq!(nine.body["code"], "BAD_REQUEST");
    assert!(
        nine.body["error"]
            .as_str()
            .is_some_and(|m| m.contains("at most 8 terms") && m.contains("9")),
        "{}",
        nine.body
    );
    Ok(())
}

#[tokio::test]
async fn a_non_current_chapter_cannot_write() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let first = ctx.seat().await?;
    let first_token = ctx.agent(&first).await?;
    ctx.ok(
        "POST",
        &mem(&first, ""),
        &first_token,
        Some(entry("kept", "project", "")),
    )
    .await?;
    let second = ctx.continue_lineage(&first, 1).await?;
    let second_token = ctx.agent(&second).await?;

    // The fenced chapter, reached by narrowing an owner token to it and by a
    // credential minted for it after the fence: both are Agent callers of
    // the right lineage, and both are refused for every write.
    let stale_token = ctx.agent(&first).await?;
    for (token, narrow) in [
        (ctx.human.as_str(), Some(first.as_str())),
        (stale_token.as_str(), None),
    ] {
        let create = ctx
            .call(
                "POST",
                &mem(&first, ""),
                token,
                narrow,
                Some(entry("late", "project", "")),
            )
            .await?;
        assert_eq!(create.status, 403, "{}", create.body);
        assert!(
            create.body["error"]
                .as_str()
                .is_some_and(|m| m.contains("current chapter")),
            "{}",
            create.body
        );
        let update = ctx
            .call(
                "PUT",
                &mem(&first, "/entries/kept"),
                token,
                narrow,
                Some(json!({"expectedRevision": 1, "body": "late"})),
            )
            .await?;
        assert_eq!(update.status, 403, "{}", update.body);
        let delete = ctx
            .call(
                "DELETE",
                &mem(&first, "/entries/kept?expectedRevision=1"),
                token,
                narrow,
                None,
            )
            .await?;
        assert_eq!(delete.status, 403, "{}", delete.body);
        // It may still read its own lineage's memory.
        let read = ctx
            .call("GET", &mem(&first, "/entries/kept"), token, narrow, None)
            .await?;
        assert_eq!(read.status, 200, "{}", read.body);
    }

    // The current chapter writes, recorded as itself.
    let written = ctx
        .ok(
            "PUT",
            &mem(&first, "/entries/kept"),
            &second_token,
            Some(json!({"expectedRevision": 1, "body": "now"})),
        )
        .await?;
    assert_eq!(written["entry"]["writerInstance"], json!(second));
    assert_eq!(written["entry"]["revision"], 2);
    Ok(())
}

#[tokio::test]
async fn another_lineages_agent_and_a_bot_are_refused_both_ways() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let mine = ctx.seat().await?;
    let theirs = ctx.seat().await?;
    let mine_token = ctx.agent(&mine).await?;
    let theirs_token = ctx.agent(&theirs).await?;
    let bot = ctx.hub.mint_bot_device_token("memory-bot").await?;
    ctx.ok(
        "POST",
        &mem(&mine, ""),
        &mine_token,
        Some(entry("private", "user", "the owner's preference")),
    )
    .await?;
    let consolidate = json!({
        "sources": [{"slug": "private", "expectedRevision": 1},
                    {"slug": "other", "expectedRevision": 1}],
        "entry": entry("merged", "user", ""),
    });
    let attempts: Vec<(&str, String, Option<Value>)> = vec![
        ("GET", mem(&mine, ""), None),
        ("GET", mem(&mine, "/entries/private"), None),
        ("GET", mem(&mine, "/index"), None),
        ("GET", mem(&mine, "/search?q=preference"), None),
        ("POST", mem(&mine, ""), Some(entry("planted", "user", ""))),
        (
            "PUT",
            mem(&mine, "/entries/private"),
            Some(json!({"expectedRevision": 1, "body": "x"})),
        ),
        (
            "DELETE",
            mem(&mine, "/entries/private?expectedRevision=1"),
            None,
        ),
        ("POST", mem(&mine, "/consolidate"), Some(consolidate)),
        ("POST", mem(&mine, "/restore"), Some(json!({}))),
    ];
    for (method, path, body) in &attempts {
        for (who, token) in [("other lineage", &theirs_token), ("bot", &bot)] {
            let reply = ctx.call(method, path, token, None, body.clone()).await?;
            assert_eq!(reply.status, 403, "{who} {method} {path}: {}", reply.body);
        }
    }
    // Narrowing an owner token to the other lineage's chapter is that Agent.
    let narrowed = ctx
        .call("GET", &mem(&mine, ""), &ctx.human, Some(&theirs), None)
        .await?;
    assert_eq!(narrowed.status, 403);
    // The owner-only routes refuse even the lineage's own seat.
    let restore = ctx
        .call(
            "POST",
            &mem(&mine, "/restore"),
            &mine_token,
            None,
            Some(json!({})),
        )
        .await?;
    assert_eq!(restore.status, 403);
    let derived = ctx
        .call(
            "GET",
            &format!("/v1/memory/derived?instanceId={mine}"),
            &mine_token,
            None,
            None,
        )
        .await?;
    assert_eq!(derived.status, 403);
    // Nothing changed.
    let entry = ctx
        .ok("GET", &mem(&mine, "/entries/private"), &ctx.human, None)
        .await?;
    assert_eq!(entry["revision"], 1);
    let listed = ctx.ok("GET", &mem(&mine, ""), &ctx.human, None).await?;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    Ok(())
}

/// r3 item 1: a Bot credential stays refused when it names a chapter with
/// `x-remuda-instance-id`: the refusal is decided on the authenticated device,
/// before narrowing. A Human narrowing to the current chapter still writes
/// as that Agent.
#[tokio::test]
async fn a_bot_cannot_reach_memory_by_naming_a_chapter() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    let bot = ctx.hub.mint_bot_device_token("memory-bot-narrowed").await?;
    for slug in ["private", "other"] {
        ctx.ok(
            "POST",
            &mem(&lineage, ""),
            &seat,
            Some(entry(slug, "user", "the owner's secret")),
        )
        .await?;
    }
    let consolidate = json!({
        "sources": [{"slug": "private", "expectedRevision": 1},
                    {"slug": "other", "expectedRevision": 1}],
        "entry": entry("merged", "user", ""),
    });
    let attempts: Vec<(&str, String, Option<Value>)> = vec![
        ("GET", mem(&lineage, ""), None),
        ("GET", mem(&lineage, "/entries/private"), None),
        ("GET", mem(&lineage, "/search?q=secret"), None),
        ("GET", mem(&lineage, "/index"), None),
        (
            "POST",
            mem(&lineage, ""),
            Some(entry("planted", "user", "")),
        ),
        (
            "PUT",
            mem(&lineage, "/entries/private"),
            Some(json!({"expectedRevision": 1, "body": "overwritten"})),
        ),
        (
            "DELETE",
            mem(&lineage, "/entries/other?expectedRevision=1"),
            None,
        ),
        ("POST", mem(&lineage, "/consolidate"), Some(consolidate)),
        ("POST", mem(&lineage, "/restore"), Some(json!({}))),
        (
            "GET",
            format!("/v1/memory/derived?instanceId={lineage}"),
            None,
        ),
    ];
    for (method, path, body) in &attempts {
        let reply = ctx
            .call(method, path, &bot, Some(&lineage), body.clone())
            .await?;
        assert_eq!(
            reply.status, 403,
            "bot narrowed to the current chapter: {method} {path}: {}",
            reply.body
        );
    }
    // Nothing moved.
    let listed = ctx.ok("GET", &mem(&lineage, ""), &ctx.human, None).await?;
    let state: Vec<(String, u64, String)> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["slug"].as_str().unwrap().to_string(),
                e["revision"].as_u64().unwrap(),
                e["body"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        state,
        [
            ("other".to_string(), 1, "the owner's secret".to_string()),
            ("private".to_string(), 1, "the owner's secret".to_string()),
        ]
    );
    // A Human narrowing to the current chapter acts as that chapter.
    let narrowed = ctx
        .call(
            "POST",
            &mem(&lineage, ""),
            &ctx.human,
            Some(&lineage),
            Some(entry("by-narrowing", "user", "")),
        )
        .await?;
    assert_eq!(narrowed.status, 200, "{}", narrowed.body);
    assert_eq!(narrowed.body["entry"]["origin"], "agent");
    assert_eq!(narrowed.body["entry"]["writerInstance"], json!(lineage));
    Ok(())
}

#[tokio::test]
async fn retiring_a_lineage_archives_and_the_owner_restores() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    for slug in ["alpha", "beta"] {
        ctx.ok(
            "POST",
            &mem(&lineage, ""),
            &seat,
            Some(entry(slug, "project", "retire marker")),
        )
        .await?;
    }
    let second = ctx.continue_lineage(&lineage, 1).await?;
    // Retirement is the whole-lineage delete of the stopped current chapter.
    ctx.hub.test_mark_instance_exited(&lineage).await?;
    ctx.hub.test_mark_instance_exited(&second).await?;
    ctx.hub
        .store()
        .context("store")?
        .delete_instance(second.clone())
        .await?
        .context("deleted")?;

    let index = ctx
        .ok("GET", &mem(&lineage, "/index"), &ctx.human, None)
        .await?;
    assert_eq!(
        (index["count"].clone(), index["text"].clone()),
        (json!(0), json!(""))
    );
    // r2 item 4: the index honours includeArchived like list and search.
    let archived_index = ctx
        .ok(
            "GET",
            &mem(&lineage, "/index?includeArchived=true"),
            &ctx.human,
            None,
        )
        .await?;
    assert_eq!(archived_index["count"], 2, "{archived_index}");
    assert_eq!(
        archived_index["text"],
        "- [project] alpha: Title alpha — About alpha (archived)\n\
         - [project] beta: Title beta — About beta (archived)\n"
    );
    assert_eq!(
        archived_index["bytes"],
        archived_index["text"].as_str().unwrap().len()
    );
    let live = ctx.ok("GET", &mem(&lineage, ""), &ctx.human, None).await?;
    assert_eq!(live["items"], json!([]));
    let hits = ctx
        .ok("GET", &mem(&lineage, "/search?q=marker"), &ctx.human, None)
        .await?;
    assert_eq!(hits["items"], json!([]));
    let archived = ctx
        .ok(
            "GET",
            &mem(&lineage, "?includeArchived=true"),
            &ctx.human,
            None,
        )
        .await?;
    let archived = archived["items"].as_array().unwrap();
    assert_eq!(archived.len(), 2, "archived, not deleted");
    assert!(archived.iter().all(|e| e["archivedAt"].is_string()));
    let hits = ctx
        .ok(
            "GET",
            &mem(&lineage, "/search?q=marker&includeArchived=true"),
            &ctx.human,
            None,
        )
        .await?;
    assert_eq!(hits["items"].as_array().unwrap().len(), 2);
    // A retired lineage takes no writes, even from the owner.
    let write = ctx
        .call(
            "POST",
            &mem(&lineage, ""),
            &ctx.human,
            None,
            Some(entry("gamma", "project", "")),
        )
        .await?;
    assert_eq!(write.status, 404, "{}", write.body);

    let restored = ctx
        .ok(
            "POST",
            &mem(&lineage, "/restore"),
            &ctx.human,
            Some(json!({})),
        )
        .await?;
    assert_eq!(restored["restored"], json!(["alpha", "beta"]));
    let index = ctx
        .ok("GET", &mem(&lineage, "/index"), &ctx.human, None)
        .await?;
    assert_eq!(index["count"], 2);
    let alpha = ctx
        .ok("GET", &mem(&lineage, "/entries/alpha"), &ctx.human, None)
        .await?;
    assert!(alpha.get("archivedAt").is_none());
    assert_eq!(alpha["revision"], 1);

    // r2 item 3: the owner can still correct and remove restored memory of
    // the retired lineage; creating new memory there stays refused.
    let edited = ctx
        .call(
            "PUT",
            &mem(&lineage, "/entries/alpha"),
            &ctx.human,
            None,
            Some(json!({"expectedRevision": 1, "body": "corrected by the owner"})),
        )
        .await?;
    assert_eq!(edited.status, 200, "owner update: {}", edited.body);
    assert_eq!(edited.body["entry"]["revision"], 2);
    assert_eq!(edited.body["entry"]["origin"], "human");
    let removed = ctx
        .call(
            "DELETE",
            &mem(&lineage, "/entries/beta?expectedRevision=1"),
            &ctx.human,
            None,
            None,
        )
        .await?;
    assert_eq!(removed.status, 200, "owner delete: {}", removed.body);
    let still_refused = ctx
        .call(
            "POST",
            &mem(&lineage, ""),
            &ctx.human,
            None,
            Some(entry("delta", "project", "")),
        )
        .await?;
    assert_eq!(still_refused.status, 404, "{}", still_refused.body);
    let left = ctx.ok("GET", &mem(&lineage, ""), &ctx.human, None).await?;
    let slugs: Vec<&str> = left["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["alpha"]);
    Ok(())
}

#[tokio::test]
async fn the_owner_finds_entries_derived_from_a_journal_row() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let other = ctx.seat().await?;
    for (target, slug, refs) in [
        (
            &lineage,
            "a",
            json!([{"instanceId": "ins_worker", "seq": 12}]),
        ),
        (
            &lineage,
            "b",
            json!([{"instanceId": "ins_worker", "seq": 13}]),
        ),
        (
            &other,
            "c",
            json!([{"instanceId": "ins_worker", "seq": 12}]),
        ),
    ] {
        let mut body = entry(slug, "project", "");
        body["sourceRefs"] = refs;
        ctx.ok("POST", &mem(target, ""), &ctx.human, Some(body))
            .await?;
    }
    let found = ctx
        .ok(
            "GET",
            "/v1/memory/derived?instanceId=ins_worker&seq=12",
            &ctx.human,
            None,
        )
        .await?;
    let mut pairs: Vec<(String, String)> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["lineageId"].as_str().unwrap().to_string(),
                e["slug"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    pairs.sort();
    let mut expected = vec![(lineage.clone(), "a".to_string()), (other, "c".to_string())];
    expected.sort();
    assert_eq!(pairs, expected);
    let all = ctx
        .ok(
            "GET",
            "/v1/memory/derived?instanceId=ins_worker",
            &ctx.human,
            None,
        )
        .await?;
    assert_eq!(all["items"].as_array().unwrap().len(), 3);
    Ok(())
}

#[tokio::test]
async fn consolidate_over_http_is_atomic() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    for slug in ["tz-1", "tz-2"] {
        ctx.ok(
            "POST",
            &mem(&lineage, ""),
            &seat,
            Some(entry(slug, "user", slug)),
        )
        .await?;
    }
    let stale = ctx
        .call(
            "POST",
            &mem(&lineage, "/consolidate"),
            &seat,
            None,
            Some(json!({
                "sources": [{"slug": "tz-1", "expectedRevision": 1},
                            {"slug": "tz-2", "expectedRevision": 7}],
                "entry": entry("tz", "user", "merged"),
            })),
        )
        .await?;
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert_eq!(
        stale.body["conflicts"],
        json!([{"slug": "tz-2", "expectedRevision": 7, "currentRevision": 1}])
    );
    let merged = ctx
        .ok(
            "POST",
            &mem(&lineage, "/consolidate"),
            &seat,
            Some(json!({
                "sources": [{"slug": "tz-1", "expectedRevision": 1},
                            {"slug": "tz-2", "expectedRevision": 1}],
                "entry": entry("tz", "user", "merged"),
            })),
        )
        .await?;
    assert_eq!(merged["removed"], json!(["tz-1", "tz-2"]));
    let listed = ctx.ok("GET", &mem(&lineage, ""), &seat, None).await?;
    let slugs: Vec<&str> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["tz"]);
    Ok(())
}

// ─── Backup and upgrade ───────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A byte-copying stand-in for `age` (`-e -r R -o OUT IN` / `-d -i K -o OUT IN`).
fn fake_age(bin: &Path) -> Result<()> {
    std::fs::create_dir_all(bin)?;
    let path = bin.join("age");
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         # Test stand-in: copies bytes; the backup test checks members, not crypto.\n\
         out=''; in=''\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -r|-i) shift 2 ;;\n\
             -e|-d) shift ;;\n\
             *) in=\"$1\"; shift ;;\n\
           esac\n\
         done\n\
         cat \"$in\" > \"$out\"\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn run(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    anyhow::ensure!(
        output.status.success(),
        "{command:?} failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(stdout)
}

#[tokio::test]
async fn hub_backup_then_restore_round_trips_memory() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    let mut tone = entry("tone", "feedback", "Why: the owner said so. 边界 first.");
    tone["sourceRefs"] = json!([{"instanceId": lineage, "seq": 9}]);
    ctx.ok("POST", &mem(&lineage, ""), &seat, Some(tone))
        .await?;
    ctx.ok(
        "PUT",
        &mem(&lineage, "/entries/tone"),
        &ctx.human,
        Some(json!({"expectedRevision": 1, "title": "Tone"})),
    )
    .await?;
    ctx.ok(
        "POST",
        &mem(&lineage, ""),
        &seat,
        Some(entry("repo", "reference", "x")),
    )
    .await?;
    let before = ctx
        .ok(
            "GET",
            &mem(&lineage, "?includeArchived=true"),
            &ctx.human,
            None,
        )
        .await?;
    let index_before = ctx
        .ok("GET", &mem(&lineage, "/index"), &ctx.human, None)
        .await?;

    let Ctx { dir, hub, .. } = ctx;
    hub.shutdown().await;
    let data = dir.path().join("hub");
    // Core members the script requires; the test Hub keeps its bootstrap in
    // config and may not have written them.
    std::fs::create_dir_all(data.join("secrets"))?;
    if !data.join("secrets/master.key").exists() {
        std::fs::write(data.join("secrets/master.key"), "test-master-key")?;
    }
    if !data.join("bootstrap-token").exists() {
        std::fs::write(data.join("bootstrap-token"), "test-bootstrap")?;
    }

    let bin = dir.path().join("bin");
    fake_age(&bin)?;
    let out = dir.path().join("backups");
    let scratch = dir.path().join("tmp");
    std::fs::create_dir_all(&scratch)?;
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let report = run(Command::new("bash")
        .arg(repo_root().join("scripts/hub-backup.sh"))
        .args(["--data-dir"])
        .arg(&data)
        .args(["--output-dir"])
        .arg(&out)
        .args(["--recipient", "age1testrecipient", "--execute"])
        .env("REMUDA_BACKUP_YES_I_KNOW", "1")
        .env("PATH", &path)
        .env("TMPDIR", &scratch))?;
    assert!(report.contains("execute ok"), "{report}");
    assert!(report.contains("[present] hub.sqlite"), "{report}");

    // Restore per hub-topology §6.1: check the sidecar digest, decrypt,
    // verify every member against the inner manifest, unpack into an empty
    // data_dir.
    let bundle = std::fs::read_dir(&out)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|ext| ext == "age"))
        .context("bundle")?;
    run(Command::new("sha256sum")
        .arg("-c")
        .arg(bundle.with_extension("age.sha256").file_name().unwrap())
        .current_dir(&out))?;
    let tar = dir.path().join("bundle.tar");
    run(Command::new(bin.join("age"))
        .args(["-d", "-i", "key", "-o"])
        .arg(&tar)
        .arg(&bundle))?;
    let staged = dir.path().join("staged");
    std::fs::create_dir_all(&staged)?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(&tar)
        .arg("-C")
        .arg(&staged))?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(staged.join("manifest.json"))?)?;
    let restored_dir = tempfile::tempdir()?;
    let restored = restored_dir.path().join("hub");
    std::fs::create_dir_all(&restored)?;
    let members = manifest["members"].as_array().context("members")?;
    assert!(members.iter().any(|m| m["path"] == "hub.sqlite"));
    for member in members {
        let rel = member["path"].as_str().context("path")?;
        let digest = run(Command::new("sha256sum").arg(rel).current_dir(&staged))?;
        assert_eq!(
            digest.split_whitespace().next(),
            member["sha256"].as_str(),
            "{rel}"
        );
        let target = restored.join(rel);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::copy(staged.join(rel), &target)?;
    }

    // The FTS index came along intact.
    {
        let db = rusqlite::Connection::open(restored.join("hub.sqlite"))?;
        db.execute(
            "INSERT INTO memory_fts(memory_fts) VALUES ('integrity-check')",
            [],
        )?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 4);
    }

    let ctx = Ctx::spawn_in(restored_dir, "hub").await?;
    let after = ctx
        .ok(
            "GET",
            &mem(&lineage, "?includeArchived=true"),
            &ctx.human,
            None,
        )
        .await?;
    assert_eq!(after, before, "entries, revisions, origin and refs survive");
    let index_after = ctx
        .ok("GET", &mem(&lineage, "/index"), &ctx.human, None)
        .await?;
    assert_eq!(index_after, index_before);
    let hits = ctx
        .ok(
            "GET",
            &mem(&lineage, "/search?q=%E8%BE%B9%E7%95%8C"),
            &ctx.human,
            None,
        )
        .await?;
    assert_eq!(hits["items"][0]["entry"]["slug"], "tone");
    let hits = ctx
        .ok("GET", &mem(&lineage, "/search?q=owner"), &ctx.human, None)
        .await?;
    assert_eq!(hits["items"][0]["matchedBy"], "fts");
    // CAS continues from the restored revision.
    let next = ctx
        .ok(
            "PUT",
            &mem(&lineage, "/entries/tone"),
            &ctx.human,
            Some(json!({"expectedRevision": 2, "body": "after restore"})),
        )
        .await?;
    assert_eq!(next["entry"]["revision"], 3);
    Ok(())
}

#[tokio::test]
async fn the_memory_step_upgrades_the_current_main_schema() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("hub");
    std::fs::create_dir_all(&data)?;
    {
        // The schema origin/main ships (user_version 3), with one lineage.
        let db = rusqlite::Connection::open(data.join("hub.sqlite"))?;
        db.execute_batch(include_str!("fixtures/hub-schema-user-version-3.sql"))?;
        db.execute(
            "INSERT INTO lineages (lineage_id, current_instance_id, generation, state, updated_at)
             VALUES ('ins_premigration', 'ins_premigration', 1, 'running',
                     '2026-10-26T00:00:00.000Z')",
            [],
        )?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 3);
        let memory: i64 = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'memory%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(memory, 0);
    }

    let ctx = Ctx::spawn_in(dir, "hub").await?;
    let lineage = "ins_premigration";
    let written = ctx
        .ok(
            "POST",
            &mem(lineage, ""),
            &ctx.human,
            Some(entry("upgraded", "project", "written after the upgrade")),
        )
        .await?;
    assert_eq!(written["entry"]["revision"], 1);
    let hits = ctx
        .ok("GET", &mem(lineage, "/search?q=upgrade"), &ctx.human, None)
        .await?;
    assert_eq!(hits["items"][0]["entry"]["slug"], "upgraded");
    let Ctx { dir, hub, .. } = ctx;
    hub.shutdown().await;
    {
        let db = rusqlite::Connection::open(dir.path().join("hub/hub.sqlite"))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 4, "one step past main, no gaps");
        let objects: Vec<String> = db
            .prepare(
                "SELECT name FROM sqlite_master
                  WHERE name IN ('memory_entries', 'memory_fts', 'memory_slug_revisions',
                                 'memory_entries_fts_insert', 'memory_entries_fts_delete',
                                 'memory_entries_fts_update')
                  ORDER BY name",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(objects.len(), 6, "{objects:?}");
        let lineage_kept: i64 = db.query_row(
            "SELECT COUNT(*) FROM lineages WHERE lineage_id = 'ins_premigration'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(lineage_kept, 1);
    }
    // Re-opening at user_version 4 runs no step again and keeps the entry.
    let ctx = Ctx::spawn_in(dir, "hub").await?;
    let entry = ctx
        .ok("GET", &mem(lineage, "/entries/upgraded"), &ctx.human, None)
        .await?;
    assert_eq!(entry["revision"], 1);
    Ok(())
}
