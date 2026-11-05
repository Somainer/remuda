use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    host: String,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (store, host) = crate::store_test_support::open_with_host(dir.path(), "search")
        .await
        .unwrap();
    Fixture { dir, store, host }
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("hub.sqlite")
}

/// A credential-shaped token assembled at run time, so no source line reads
/// as one (the secret scanner reads this file too).
fn secret(parts: &[&str]) -> String {
    parts.concat()
}

fn message(node: &str, operation: &str, role: &str, text: &str) -> Value {
    json!({
        "kind": "message",
        "payload": {
            "nodeId": node, "operation": operation, "messageId": node,
            "role": role, "phase": "final",
            "blocks": [{"type": "text", "text": text}],
        },
    })
}

fn tool_call(node: &str, name: &str, input: Value) -> Value {
    json!({
        "kind": "tool_call",
        "payload": {
            "nodeId": node, "operation": "close", "toolCallId": node,
            "toolName": {"state": "known", "value": name},
            "displayTitle": {"state": "known", "value": name},
            "input": {"state": "known", "value": input},
        },
    })
}

fn tool_result(node: &str, stage: &str, text: &str) -> Value {
    json!({
        "kind": "tool_result",
        "payload": {
            "nodeId": node, "operation": "open", "stage": stage,
            "blocks": [{"type": "text", "text": text}],
        },
    })
}

fn human() -> SearchScope {
    SearchScope::default()
}

fn agent(instance: &str) -> SearchScope {
    SearchScope {
        agent_instance: Some(instance.to_string()),
        ..SearchScope::default()
    }
}

impl Fixture {
    fn db(&self) -> Connection {
        Connection::open(db_path(self.dir.path())).unwrap()
    }

    async fn instance(&self) -> String {
        self.store
            .insert_instance(
                self.host.clone(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"kind": "claude"}),
            )
            .await
            .unwrap()
            .instance_id
    }

    /// A chapter of `lineage`, dispatched by `parent` when given.
    async fn chapter(&self, lineage: Option<&str>, parent: Option<&str>) -> String {
        let id = self.instance().await;
        let db = self.db();
        if let Some(lineage) = lineage {
            db.execute(
                "UPDATE instances SET lineage_id = ?1 WHERE id = ?2",
                params![lineage, id],
            )
            .unwrap();
        }
        if let Some(parent) = parent {
            db.execute(
                "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
                params![
                    json!({"kind": "claude", "parentInstanceId": parent}).to_string(),
                    id
                ],
            )
            .unwrap();
        }
        id
    }

    async fn append(&self, instance: &str, event: Value) -> i64 {
        self.store
            .append_journal(self.host.clone(), instance.to_string(), None, event)
            .await
            .unwrap()
            .record
            .seq
    }

    async fn search(&self, scope: SearchScope, query: &str) -> JournalSearchResult {
        self.search_limit(scope, query, 20).await
    }

    async fn search_limit(
        &self,
        scope: SearchScope,
        query: &str,
        limit: usize,
    ) -> JournalSearchResult {
        self.store
            .journal_search(scope, query.to_string(), limit, QUERY_BUDGET)
            .await
            .unwrap()
    }

    fn docs(&self) -> i64 {
        self.db()
            .query_row("SELECT COUNT(*) FROM journal_search_docs", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// A journal row written the way rows older than the index were: straight
    /// into the table, never through the append path that indexes it.
    fn raw_row(&self, db: &Connection, instance: &str, seq: i64, event: &Value) {
        db.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, '2026-11-01T00:00:00.000Z')",
            params![
                instance,
                seq,
                format!("evt-{instance}-{seq}"),
                event.to_string()
            ],
        )
        .unwrap();
    }
}

fn seqs(result: &JournalSearchResult) -> Vec<(String, u64)> {
    result
        .hits
        .iter()
        .map(|hit| (hit.instance_id.clone(), hit.seq.0))
        .collect()
}

#[test]
fn extract_takes_complete_text_and_skips_deltas_thoughts_and_progress() {
    let doc = extract(&message("n1", "replace", "assistant", "the full reply")).unwrap();
    assert_eq!(doc.kind, JournalSearchKind::Message);
    assert_eq!(doc.role.as_deref(), Some("assistant"));
    assert_eq!(doc.text, "the full reply");
    assert_eq!(doc.node_id, "n1");
    // A streamed delta, a thought, a non-final result and a system message
    // carry nothing to index.
    assert!(extract(&message("n1", "append", "assistant", "the")).is_none());
    assert!(extract(&message("n2", "close", "system", "boot")).is_none());
    let mut thought = message("n3", "close", "assistant", "pondering");
    thought["kind"] = json!("thought");
    assert!(extract(&thought).is_none());
    assert!(extract(&tool_result("r1", "progress", "50%")).is_none());
    let result = extract(&tool_result("r1", "final", "added wak_1 (rc-wake)")).unwrap();
    assert_eq!(result.kind, JournalSearchKind::ToolResult);
    // The tool name and the input's values, not its keys.
    let call = extract(&tool_call(
        "t1",
        "Bash",
        json!({"command": "remuda wake add --name rc", "description": "Add a wake"}),
    ))
    .unwrap();
    assert_eq!(call.text, "Bash remuda wake add --name rc Add a wake");
    // The Hub's agent-to-agent record carries `text`, not blocks.
    let hub = extract(&json!({
        "kind": "message", "origin": "hub",
        "payload": {"role": "user", "origin": "agent", "text": "please rebase"},
    }))
    .unwrap();
    assert_eq!(hub.text, "please rebase");
    assert_eq!(hub.node_id, "");
}

#[test]
fn extract_bounds_tool_inputs_and_results() {
    let long = "word ".repeat(5000);
    let call = extract(&tool_call("t1", "Bash", json!({"command": long}))).unwrap();
    assert!(
        call.text.len() <= TOOL_TEXT_BYTES + "Bash ".len(),
        "{}",
        call.text.len()
    );
    let result = extract(&tool_result("r1", "final", &long)).unwrap();
    assert!(
        result.text.len() <= TOOL_TEXT_BYTES,
        "{}",
        result.text.len()
    );
    let message = extract(&message("m1", "close", "user", &"字".repeat(20_000))).unwrap();
    assert!(message.text.len() <= MESSAGE_TEXT_BYTES);
    assert!(message.text.is_char_boundary(message.text.len()));
}

#[tokio::test]
async fn a_streamed_message_is_one_hit_at_its_newest_revision() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    fx.append(&ins, message("m1", "open", "assistant", "Rolling"))
        .await;
    fx.append(&ins, message("m1", "append", "assistant", " back"))
        .await;
    let replace = fx
        .append(
            &ins,
            message("m1", "replace", "assistant", "Rolling back the migration"),
        )
        .await;
    let close = fx
        .append(
            &ins,
            message("m1", "close", "assistant", "Rolling back the migration"),
        )
        .await;
    assert!(close > replace);
    let found = fx.search(human(), "rolling").await;
    assert_eq!(seqs(&found), vec![(ins.clone(), close as u64)]);
    assert_eq!(found.hits[0].matched_by, JournalSearchMatch::Fts);
    assert_eq!(found.hits[0].snippet, "Rolling back the migration");
    // A late older revision (a replay below the close) never replaces it.
    let db = fx.db();
    assert!(
        !index_event(
            &db,
            &ins,
            replace,
            &message("m1", "replace", "assistant", "Rolling"),
            "2026-11-01T00:00:00.000Z"
        )
        .unwrap()
    );
    assert_eq!(fx.docs(), 1);
}

#[tokio::test]
async fn new_rows_are_found_immediately_and_fts_syntax_is_literal() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    let seq = fx
        .append(
            &ins,
            message("m1", "close", "user", "migrate 0042 AND NOT drop"),
        )
        .await;
    // Found in the same moment the append returned: no backfill involved.
    let found = fx.search(human(), "0042").await;
    assert_eq!(seqs(&found), vec![(ins.clone(), seq as u64)]);
    // FTS operators, quotes, column filters and prefixes are data.
    for query in [
        "AND",
        "NOT drop",
        "\"0042",
        "text:migrate",
        "migr*",
        "(drop",
    ] {
        let result = fx.search(human(), query).await;
        assert!(!result.partial, "{query}");
        assert!(
            result.hits.iter().all(|hit| hit.seq.0 == seq as u64),
            "{query}: {:?}",
            seqs(&result)
        );
    }
    assert_eq!(fx.search(human(), "NOT drop").await.hits.len(), 1);
    assert!(fx.search(human(), "migr*").await.hits.is_empty());
    assert!(fx.search(human(), "OR").await.hits.is_empty());
}

#[tokio::test]
async fn a_two_character_chinese_query_hits_through_like() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    let seq = fx
        .append(
            &ins,
            message(
                "m1",
                "close",
                "user",
                "旧迁移 0042 删掉还是保留？先看边界条件",
            ),
        )
        .await;
    // unicode61 makes the CJK run one token: FTS alone cannot see 边界.
    let db = fx.db();
    let fts = fts_pass(&db, &human(), None, &["边界"], 10).unwrap();
    assert!(fts.is_empty());
    let found = fx.search(human(), "边界").await;
    assert_eq!(seqs(&found), vec![(ins.clone(), seq as u64)]);
    assert_eq!(found.hits[0].matched_by, JournalSearchMatch::Like);
    assert!(found.hits[0].snippet.contains("边界"));
    // LIKE wildcards in the query are literal too.
    assert!(fx.search(human(), "边%").await.hits.is_empty());
    assert!(fx.search(human(), "保_").await.hits.is_empty());
}

#[tokio::test]
async fn a_redacted_secret_is_never_findable() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    let key = secret(&["sk", "-ant-api03-AAAAAAAAAAAAAAAAAAAAQQ"]);
    let gh = secret(&["ghp", "_abcdefghijklmnopqrstuvwxyz0123456789"]);
    fx.append(
        &ins,
        message("m1", "close", "user", &format!("deploy with {key} now")),
    )
    .await;
    fx.append(
        &ins,
        tool_call(
            "t1",
            "Bash",
            json!({"command": format!("export GH_TOKEN={gh}")}),
        ),
    )
    .await;
    fx.append(&ins, tool_result("r1", "final", &format!("token: {gh}")))
        .await;
    for query in [
        key.as_str(),
        gh.as_str(),
        "AAAAAAAAAAAAAAAAAAAAQQ",
        "abcdefghijklmnopqrstuvwxyz0123456789",
    ] {
        let result = fx.search(human(), query).await;
        assert!(result.hits.is_empty(), "{query}: {:?}", seqs(&result));
    }
    // The rows are indexed (the words around the secret are found), and no
    // snippet or stored text carries it.
    let around = fx.search(human(), "deploy").await;
    assert_eq!(around.hits.len(), 1);
    assert!(!around.hits[0].snippet.contains("AAAAAAAAAAAAAAAAAAAA"));
    let stored: Vec<String> = fx
        .db()
        .prepare("SELECT text FROM journal_search_docs")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(stored.len(), 3);
    for text in stored {
        assert!(!text.contains(&key) && !text.contains(&gh), "{text}");
    }
}

#[tokio::test]
async fn sixty_foreign_hits_ranked_above_never_hide_the_one_own_hit() {
    let fx = fixture().await;
    let seat = fx.instance().await;
    let worker = fx.chapter(None, Some(&seat)).await;
    let foreign = fx.instance().await;
    // 60 foreign rows that rank above the own row: the term is dense in them.
    for n in 0..60 {
        fx.append(
            &foreign,
            message(
                &format!("f{n}"),
                "close",
                "assistant",
                "rollback rollback rollback",
            ),
        )
        .await;
    }
    let own = fx
        .append(
            &worker,
            message(
                "w1",
                "close",
                "assistant",
                "a long report that mentions rollback once among many other words here",
            ),
        )
        .await;
    // The owner's world sees them all, foreign first, at the limit.
    let all = fx.search_limit(human(), "rollback", 50).await;
    assert_eq!(all.hits.len(), 50);
    assert!(all.hits.iter().all(|hit| hit.instance_id == foreign));
    // The seat's search applies the predicate before the limit: its one hit.
    let mine = fx.search_limit(agent(&seat), "rollback", 50).await;
    assert_eq!(seqs(&mine), vec![(worker.clone(), own as u64)]);
    assert_eq!(mine.hits[0].lineage_id, worker);
    // The foreign instance's own Agent sees its rows, never the worker's.
    let theirs = fx.search_limit(agent(&foreign), "rollback", 50).await;
    assert_eq!(theirs.hits.len(), 50);
    assert!(theirs.hits.iter().all(|hit| hit.instance_id == foreign));
    // The same holds for the substring pass.
    let mine = fx.search_limit(agent(&seat), "ro", 50).await;
    assert_eq!(seqs(&mine), vec![(worker, own as u64)]);
}

#[tokio::test]
async fn scope_follows_the_lineage_edge_and_narrowing_never_widens() {
    let fx = fixture().await;
    let first = fx.instance().await;
    // A second chapter of the seat's lineage, a worker the first chapter
    // dispatched, its own sub-worker, and an unrelated lineage.
    let second = fx.chapter(Some(&first), None).await;
    let worker = fx.chapter(None, Some(&first)).await;
    let grandchild = fx.chapter(None, Some(&worker)).await;
    let other = fx.instance().await;
    let mut at = Vec::new();
    for ins in [&first, &second, &worker, &grandchild, &other] {
        at.push(
            fx.append(
                ins,
                message("m", "close", "assistant", "checkpoint reached"),
            )
            .await,
        );
    }
    let ids = |result: JournalSearchResult| {
        let mut ids: Vec<String> = result.hits.into_iter().map(|hit| hit.instance_id).collect();
        ids.sort();
        ids
    };
    let sorted = |list: &[&String]| {
        let mut list: Vec<String> = list.iter().map(|id| id.to_string()).collect();
        list.sort();
        list
    };
    // `owns()`: the lineage's chapters and instances whose parent is in it,
    // from either chapter. The worker's own child is one hop too far.
    for seat in [&first, &second] {
        assert_eq!(
            ids(fx.search(agent(seat), "checkpoint").await),
            sorted(&[&first, &second, &worker])
        );
    }
    // The worker reads itself and its child, never its parent's lineage.
    assert_eq!(
        ids(fx.search(agent(&worker), "checkpoint").await),
        sorted(&[&worker, &grandchild])
    );
    // Narrowing by instance, lineage and time, inside and outside the scope.
    let narrowed = |instance: Option<&str>, lineage: Option<&str>, since: Option<&str>| {
        let mut scope = agent(&first);
        scope.instance = instance.map(str::to_string);
        scope.lineage = lineage.map(str::to_string);
        scope.since = since.map(str::to_string);
        scope
    };
    assert_eq!(
        ids(fx
            .search(narrowed(Some(&second), None, None), "checkpoint")
            .await),
        sorted(&[&second])
    );
    assert_eq!(
        ids(fx
            .search(narrowed(None, Some(&first), None), "checkpoint")
            .await),
        sorted(&[&first, &second])
    );
    assert!(
        fx.search(narrowed(Some(&other), None, None), "checkpoint")
            .await
            .hits
            .is_empty()
    );
    assert!(
        fx.search(narrowed(None, Some(&other), None), "checkpoint")
            .await
            .hits
            .is_empty()
    );
    assert!(
        fx.search(
            narrowed(None, None, Some("2999-01-01T00:00:00.000Z")),
            "checkpoint"
        )
        .await
        .hits
        .is_empty()
    );
    // A Human reads every instance; narrowing narrows it.
    assert_eq!(fx.search(human(), "checkpoint").await.hits.len(), 5);
    let mut scope = human();
    scope.lineage = Some(other.clone());
    assert_eq!(ids(fx.search(scope, "checkpoint").await), sorted(&[&other]));
    // A caller row that is gone reads nothing.
    assert!(
        fx.search(agent("ins_gone"), "checkpoint")
            .await
            .hits
            .is_empty()
    );
}

#[tokio::test]
async fn purging_an_instance_removes_its_hits() {
    let fx = fixture().await;
    let gone = fx.instance().await;
    let kept = fx.instance().await;
    fx.append(
        &gone,
        message("m", "close", "user", "ephemeral phrase 甲乙"),
    )
    .await;
    fx.append(
        &kept,
        message("m", "close", "user", "ephemeral phrase kept"),
    )
    .await;
    assert_eq!(fx.search(human(), "ephemeral").await.hits.len(), 2);
    fx.store
        .purge_requested_instance(gone.clone())
        .await
        .unwrap();
    let left = fx.search(human(), "ephemeral").await;
    assert_eq!(left.hits.len(), 1);
    assert_eq!(left.hits[0].instance_id, kept);
    assert!(fx.search(human(), "甲乙").await.hits.is_empty());
    // Nothing of it is left in the index or its FTS table.
    let db = fx.db();
    let docs: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM journal_search_docs WHERE instance_id = ?1",
            [&gone],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(docs, 0);
    let fts: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM journal_fts WHERE journal_fts MATCH 'ephemeral'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fts, 1);
}

/// Rows written before the index existed are found only after the backfill;
/// opening the Store adds no index rows; a batch that dies mid-way commits
/// nothing and the next one picks the same rows up; every row is indexed
/// exactly once.
#[tokio::test]
async fn backfill_indexes_old_rows_in_resumable_batches_and_boot_adds_none() {
    let fx = fixture().await;
    let a = fx.instance().await;
    let b = fx.instance().await;
    {
        let db = fx.db();
        for seq in 1..=7 {
            fx.raw_row(
                &db,
                &a,
                seq,
                &message(
                    &format!("a{seq}"),
                    "close",
                    "user",
                    &format!("legacy note {seq}"),
                ),
            );
            // Deltas and thoughts are read but contribute nothing.
            fx.raw_row(
                &db,
                &b,
                seq * 2 - 1,
                &message("b", "append", "assistant", "legacy"),
            );
            fx.raw_row(
                &db,
                &b,
                seq * 2,
                &message(
                    &format!("b{seq}"),
                    "close",
                    "assistant",
                    &format!("legacy reply {seq}"),
                ),
            );
        }
        db.execute_batch(
            "UPDATE instances SET durable_seq = 7 WHERE durable_seq = 0 AND id IN (SELECT instance_id FROM journal);",
        )
        .unwrap();
    }
    // Reopen the database below the search rung, as an upgrade finds it.
    let Fixture { dir, store, host } = fx;
    drop(store);
    {
        let db = Connection::open(db_path(dir.path())).unwrap();
        db.execute_batch(
            "DROP TABLE journal_search_backfill;
             DROP TRIGGER journal_search_purge;
             DROP TABLE journal_fts;
             DROP TABLE journal_search_docs;
             PRAGMA user_version = 6;",
        )
        .unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    let fx = Fixture { dir, store, host };
    let version: i64 = fx
        .db()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SEARCH_SCHEMA_VERSION);
    assert_eq!(fx.docs(), 0, "the migration adds no rows");
    let before = fx.search(human(), "legacy").await;
    assert!(before.hits.is_empty() && !before.index_complete);
    let status = fx.store.journal_search_status(true).await.unwrap();
    assert_eq!(status.state, JournalSearchBackfillState::Pending);
    assert_eq!(status.journal_rows, Some(U64(21)));

    // A row appended now is indexed by its append, before the backfill.
    let fresh = fx
        .append(&a, message("a-new", "close", "user", "legacy follow-up"))
        .await;
    assert_eq!(fresh, 8);
    assert_eq!(
        seqs(&fx.search(human(), "follow").await),
        vec![(a.clone(), 8)]
    );

    // Batch 1 (5 rows) dies on its 4th row: nothing of it commits.
    let mut db = fx.db();
    test_seam::FAIL_AT.with(|fail| fail.set(Some(3)));
    assert!(backfill_batch(&mut db, 5).is_err());
    assert_eq!(fx.docs(), 1);
    assert_eq!(backfill_mark(&db).unwrap().scanned, 0);
    // The rerun reads the same rows; the batches walk the key order to the end.
    let mut steps = Vec::new();
    loop {
        let step = backfill_batch(&mut db, 5).unwrap();
        steps.push(step);
        if step.done {
            break;
        }
    }
    let scanned: usize = steps.iter().map(|step| step.scanned).sum();
    let indexed: usize = steps.iter().map(|step| step.indexed).sum();
    // 22 journal rows; 14 carry text (7 + 7), the fresh row was already there.
    assert_eq!(scanned, 22);
    assert_eq!(indexed, 14);
    assert_eq!(fx.docs(), 15);
    // Finished: a further batch is a no-op and the status says so.
    assert_eq!(
        backfill_batch(&mut db, 5).unwrap(),
        BackfillStep {
            done: true,
            ..BackfillStep::default()
        }
    );
    let status = fx.store.journal_search_status(true).await.unwrap();
    assert!(status.complete);
    assert_eq!(status.state, JournalSearchBackfillState::Done);
    assert_eq!(status.scanned_rows, Some(U64(22)));
    assert_eq!(status.indexed_rows, Some(U64(14)));
    assert_eq!(status.docs, Some(U64(15)));
    let after = fx.search_limit(human(), "legacy", 50).await;
    assert!(after.index_complete);
    assert_eq!(after.hits.len(), 15);
    // The Agent-facing status carries no Hub-wide counts.
    let narrow = fx.store.journal_search_status(false).await.unwrap();
    assert!(narrow.complete && narrow.docs.is_none() && narrow.journal_rows.is_none());
}

#[tokio::test]
async fn backfill_resumes_from_its_mark_after_a_restart() {
    let fx = fixture().await;
    let a = fx.instance().await;
    {
        let db = fx.db();
        for seq in 1..=9 {
            fx.raw_row(
                &db,
                &a,
                seq,
                &message(
                    &format!("m{seq}"),
                    "close",
                    "user",
                    &format!("resumable {seq}"),
                ),
            );
        }
    }
    let mut db = fx.db();
    assert_eq!(backfill_batch(&mut db, 4).unwrap().scanned, 4);
    drop(db);
    let Fixture { dir, store, host } = fx;
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let fx = Fixture { dir, store, host };
    // The spawned job (the Hub's boot path) finishes from the mark.
    spawn_backfill(fx.store.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fx.store.journal_search_status(true).await.unwrap();
        if status.complete {
            assert_eq!(status.scanned_rows, Some(U64(9)));
            assert_eq!(status.indexed_rows, Some(U64(9)));
            break;
        }
        assert!(Instant::now() < deadline, "backfill did not finish");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fx.search(human(), "resumable").await.hits.len(), 9);
}

#[tokio::test]
async fn a_query_past_its_budget_is_partial_not_an_error() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    for n in 0..200 {
        fx.append(
            &ins,
            message(&format!("m{n}"), "close", "user", "budget 预算 words"),
        )
        .await;
    }
    let result = fx
        .store
        .journal_search(human(), "预算".into(), 50, Duration::ZERO)
        .await
        .unwrap();
    assert!(result.partial);
    // The budget is disarmed afterwards: the pooled connection answers.
    let result = fx.search_limit(human(), "预算", 50).await;
    assert!(!result.partial);
    assert_eq!(result.hits.len(), 50);
}

#[test]
fn snippets_are_bounded_and_centred_on_the_match() {
    let text = format!("{} needle {}", "a ".repeat(300), "b ".repeat(300));
    let snippet = snippet_around(&text, &["NEEDLE"]);
    assert_eq!(snippet.chars().count(), JOURNAL_SEARCH_SNIPPET_CHARS);
    assert!(snippet.contains("needle"));
    assert!(snippet.starts_with('…') && snippet.ends_with('…'));
    assert_eq!(
        snippet_around("short 边界 text", &["边界"]),
        "short 边界 text"
    );
    assert_eq!(clip_chars(&"x".repeat(500), 200).chars().count(), 200);
}
