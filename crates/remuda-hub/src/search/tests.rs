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

fn thought(node: &str, text: &str) -> Value {
    let mut event = message(node, "append", "assistant", text);
    event["kind"] = json!("thought");
    event
}

/// A PTY/print-shaped message row: one text block, appends target block 0.
fn streamed(node: &str, operation: &str, text: &str) -> Value {
    json!({
        "kind": "message",
        "payload": {
            "nodeId": node, "operation": operation, "messageId": node,
            "role": "assistant", "phase": "final", "targetBlock": 0,
            "blocks": [{"type": "text", "text": text}],
        },
    })
}

/// A call row whose input is still streaming (`inputTextDelta`).
fn call_delta(node: &str, operation: &str, name: &str, delta: &str) -> Value {
    json!({
        "kind": "tool_call",
        "payload": {
            "nodeId": node, "operation": operation, "toolCallId": node,
            "toolName": {"state": "known", "value": name},
            "displayTitle": {"state": "known", "value": name},
            "input": {"state": "unknown", "reason": "streaming-input"},
            "inputTextDelta": delta,
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

    fn doc_texts(&self) -> Vec<(String, String)> {
        self.db()
            .prepare("SELECT kind, text FROM journal_search_docs ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
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
fn extract_reads_each_rows_text_and_skips_thoughts_and_progress() {
    let piece = extract(&message("n1", "replace", "assistant", "the full reply")).unwrap();
    assert_eq!(piece.kind, JournalSearchKind::Message);
    assert_eq!(piece.mode, PieceMode::Snapshot);
    assert_eq!(piece.role.as_deref(), Some("assistant"));
    assert_eq!(piece.text, "the full reply");
    assert_eq!(piece.node_id, "n1");
    // A delta is a piece too: streaming carriers send most text that way.
    let delta = extract(&message("n1", "append", "assistant", " more")).unwrap();
    assert_eq!(delta.mode, PieceMode::Delta);
    assert_eq!(delta.text, " more");
    // A thought, a non-final result and a system message carry nothing.
    assert!(extract(&message("n2", "close", "system", "boot")).is_none());
    let mut thought = message("n3", "close", "assistant", "pondering");
    thought["kind"] = json!("thought");
    assert!(extract(&thought).is_none());
    assert!(extract(&tool_result("r1", "progress", "50%")).is_none());
    let result = extract(&tool_result("r1", "final", "added wak_1 (rc-wake)")).unwrap();
    assert_eq!(result.kind, JournalSearchKind::ToolResult);
    // A call: its name, and the input's values, not its keys.
    let call = extract(&tool_call(
        "t1",
        "Bash",
        json!({"command": "remuda wake add --name rc", "description": "Add a wake"}),
    ))
    .unwrap();
    assert_eq!(call.head.as_deref(), Some("Bash"));
    assert_eq!(
        call.input.as_deref(),
        Some("remuda wake add --name rc Add a wake")
    );
    // The Hub's agent-to-agent record carries `text`, not blocks.
    let hub = extract(&json!({
        "kind": "message", "origin": "hub",
        "payload": {"role": "user", "origin": "agent", "text": "please rebase"},
    }))
    .unwrap();
    assert_eq!(hub.text, "please rebase");
    assert_eq!(hub.node_id, "");
}

#[tokio::test]
async fn documents_are_bounded() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    let long = "word ".repeat(5000);
    fx.append(&ins, tool_call("t1", "Bash", json!({"command": long})))
        .await;
    fx.append(&ins, tool_result("r1", "final", &long)).await;
    fx.append(&ins, message("m1", "close", "user", &"字".repeat(20_000)))
        .await;
    // A stream of deltas is bounded the same way.
    fx.append(&ins, message("m2", "open", "assistant", "start"))
        .await;
    for _ in 0..200 {
        fx.append(&ins, message("m2", "append", "assistant", &"x".repeat(400)))
            .await;
    }
    for (kind, text) in fx.doc_texts() {
        let cap = match kind.as_str() {
            "message" => MESSAGE_TEXT_BYTES,
            _ => TOOL_TEXT_BYTES + "Bash ".len(),
        };
        assert!(text.len() <= cap, "{kind}: {}", text.len());
        assert!(text.is_char_boundary(text.len()));
    }
}

/// A claude-pty-shaped stream: the PTY message fold sends the first chunk
/// as `open` and every later one, the last included, as an `append` that
/// carries only its own delta.
#[tokio::test]
async fn a_pty_stream_is_found_by_a_word_from_its_last_delta() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    fx.append(&ins, streamed("m1", "open", "I'll ")).await;
    fx.append(&ins, streamed("m1", "append", "check the "))
        .await;
    let last = fx
        .append(&ins, streamed("m1", "append", "rollback of migration 0042"))
        .await;
    let found = fx.search(human(), "rollback 0042").await;
    assert_eq!(seqs(&found), vec![(ins.clone(), last as u64)]);
    assert_eq!(
        found.hits[0].snippet,
        "I'll check the rollback of migration 0042"
    );
    // A phrase across two deltas is one document's text.
    assert_eq!(fx.search(human(), "the rollback").await.hits.len(), 1);
    assert_eq!(fx.docs(), 1);
}

/// claude-print partial streaming: the first delta opens, the rest append,
/// and the block's `close` carries only its last delta.
#[tokio::test]
async fn a_print_stream_is_one_document_and_its_close_adds_no_copy() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    fx.append(&ins, streamed("m1", "open", "Rolling")).await;
    for delta in [" back", " the", " quartermaster", " ledger"] {
        fx.append(&ins, streamed("m1", "append", delta)).await;
    }
    let close = fx.append(&ins, streamed("m1", "close", " ledger")).await;
    let found = fx.search(human(), "quartermaster").await;
    assert_eq!(seqs(&found), vec![(ins.clone(), close as u64)]);
    assert_eq!(
        found.hits[0].snippet,
        "Rolling back the quartermaster ledger"
    );
    assert_eq!(fx.docs(), 1);
    // A close that re-emits the whole text adds nothing either.
    fx.append(
        &ins,
        streamed("m1", "close", "Rolling back the quartermaster ledger"),
    )
    .await;
    assert_eq!(fx.docs(), 1);
}

/// A prompt-correlation restamp replaces a node's text with different
/// text: the journal still returns both rows, so both stay findable.
#[tokio::test]
async fn a_replace_with_different_text_keeps_the_old_text_findable() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    let opened = fx
        .append(
            &ins,
            message("u1", "open", "user", "please revert migration 0042"),
        )
        .await;
    let replaced = fx
        .append(
            &ins,
            message("u1", "replace", "user", "please revert migration 0099"),
        )
        .await;
    assert_eq!(
        seqs(&fx.search(human(), "0042").await),
        vec![(ins.clone(), opened as u64)]
    );
    assert_eq!(
        seqs(&fx.search(human(), "0099").await),
        vec![(ins.clone(), replaced as u64)]
    );
    // A replace that only extends the text is the same document.
    let extended = fx
        .append(
            &ins,
            message(
                "u1",
                "replace",
                "user",
                "please revert migration 0099 today",
            ),
        )
        .await;
    assert_eq!(
        seqs(&fx.search(human(), "today").await),
        vec![(ins.clone(), extended as u64)]
    );
    assert_eq!(fx.docs(), 2);
}

/// codex and grok stamp a call's result with the call's node id: the call's
/// name and input and the result's text are each findable.
#[tokio::test]
async fn a_call_and_its_result_sharing_a_node_id_are_both_findable() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    let call = fx
        .append(
            &ins,
            tool_call("item_7", "shell", json!({"command": "git status --short"})),
        )
        .await;
    let result = fx
        .append(
            &ins,
            tool_result("item_7", "final", "nothing to commit, working tree clean"),
        )
        .await;
    assert_eq!(
        seqs(&fx.search(human(), "git status").await),
        vec![(ins.clone(), call as u64)]
    );
    assert_eq!(
        seqs(&fx.search(human(), "working tree clean").await),
        vec![(ins.clone(), result as u64)]
    );
    // A streamed call's input deltas give way to its complete input.
    fx.append(&ins, call_delta("item_9", "open", "Bash", ""))
        .await;
    fx.append(
        &ins,
        call_delta("item_9", "append", "Bash", "{\"command\": \"cargo"),
    )
    .await;
    let closed = fx
        .append(
            &ins,
            tool_call("item_9", "Bash", json!({"command": "cargo test -p remuda"})),
        )
        .await;
    let found = fx.search(human(), "cargo").await;
    assert_eq!(seqs(&found), vec![(ins.clone(), closed as u64)]);
    assert_eq!(found.hits[0].snippet, "Bash cargo test -p remuda");
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
async fn a_secret_split_across_two_deltas_is_never_findable() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    // Neither delta is credential-shaped; their concatenation is.
    fx.append(&ins, streamed("m1", "open", "the key is sk"))
        .await;
    fx.append(
        &ins,
        streamed(
            "m1",
            "append",
            &secret(&["-ant-api03-", "BBBBBBBBBBBBBBBBBBBBQQ"]),
        ),
    )
    .await;
    fx.append(&ins, streamed("m1", "append", " keep it safe"))
        .await;
    for query in ["BBBBBBBBBBBBBBBBBBBBQQ", "api03", "BBBB"] {
        let result = fx.search(human(), query).await;
        assert!(result.hits.is_empty(), "{query}: {:?}", seqs(&result));
    }
    assert_eq!(fx.search(human(), "safe").await.hits.len(), 1);
    for (_, text) in fx.doc_texts() {
        assert!(!text.contains("BBBBBBBBBBBBBBBBBBBB"), "{text}");
    }
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
            // Thoughts are read but contribute nothing.
            fx.raw_row(&db, &b, seq * 2 - 1, &thought("b", "legacy"));
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
             DROP TABLE journal_search_rows;
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

/// Rows covering every fold rule, in journal order: a PTY stream, a print
/// stream closed with its last delta, a restamped prompt, a call and its
/// result on one node id, a streamed call, two Hub text rows (no node), a
/// thought, and interleaving between nodes.
fn mixed_rows() -> Vec<Value> {
    vec![
        message("u1", "open", "user", "please revert migration 0042"),
        streamed("a1", "open", "I'll "),
        streamed("a1", "append", "check "),
        call_delta("c1", "open", "Bash", ""),
        streamed("a1", "append", "the rollback"),
        call_delta("c1", "append", "Bash", "{\"command\": \"git"),
        message("u1", "replace", "user", "please revert migration 0099"),
        tool_call("c1", "Bash", json!({"command": "git log --oneline"})),
        tool_result("c1", "final", "abc123 revert 0042"),
        json!({"kind": "message", "origin": "hub",
               "payload": {"role": "user", "origin": "agent", "text": "hub note one"}}),
        streamed("a2", "open", "Done"),
        streamed("a2", "append", " with the"),
        thought("t1", "pondering"),
        streamed("a2", "append", " ledger"),
        streamed("a2", "close", " ledger"),
        json!({"kind": "message", "origin": "hub",
               "payload": {"role": "user", "origin": "agent", "text": "hub note two"}}),
        message("u1", "replace", "user", "please revert migration 0099 now"),
        streamed("a1", "close", "I'll check the rollback"),
    ]
}

/// A node's documents as comparable values (no ids, no instance).
type DocKey = (
    String,
    String,
    i64,
    i64,
    Option<String>,
    String,
    String,
    i64,
    String,
);

impl Fixture {
    fn docs_of(&self, instance: &str) -> Vec<DocKey> {
        self.db()
            .prepare(
                "SELECT kind, node_id, first_seq, seq, role, head, raw, indexed_len, text
                   FROM journal_search_docs WHERE instance_id = ?1
                  ORDER BY kind, node_id, first_seq",
            )
            .unwrap()
            .query_map([instance], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
}

/// The append path and the backfill build the same documents from the same
/// rows, and so does a mix: rows written before the index existed, then
/// rows appended live, then the backfill meeting both (a node that straddles
/// the migration is rebuilt in seq order).
#[tokio::test]
async fn the_append_path_and_the_backfill_build_identical_documents() {
    let fx = fixture().await;
    let rows = mixed_rows();
    let observed = |seq: usize| format!("2026-11-01T00:00:{:02}.000Z", seq);
    // Live: every row through the append path.
    let live = fx.instance().await;
    for event in &rows {
        fx.append(&live, event.clone()).await;
    }
    let db = fx.db();
    // Normalise observed_at, which the append path stamps with the clock.
    for (n, _) in rows.iter().enumerate() {
        db.execute(
            "UPDATE journal SET observed_at = ?1 WHERE instance_id = ?2 AND seq = ?3",
            params![observed(n + 1), live, (n + 1) as i64],
        )
        .unwrap();
    }
    // Backfill: every row written raw, indexed by the backfill.
    let backfilled = fx.instance().await;
    // Straddle: the first half raw, the rest through the append path.
    let straddled = fx.instance().await;
    let half = rows.len() / 2;
    for (n, event) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                backfilled,
                (n + 1) as i64,
                format!("evt-b-{n}"),
                event.to_string(),
                observed(n + 1)
            ],
        )
        .unwrap();
        if n < half {
            db.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    straddled,
                    (n + 1) as i64,
                    format!("evt-s-{n}"),
                    event.to_string(),
                    observed(n + 1)
                ],
            )
            .unwrap();
        }
    }
    db.execute(
        "UPDATE instances SET durable_seq = ?1 WHERE id = ?2",
        params![half as i64, straddled],
    )
    .unwrap();
    for event in &rows[half..] {
        fx.append(&straddled, event.clone()).await;
    }
    for n in half..rows.len() {
        db.execute(
            "UPDATE journal SET observed_at = ?1 WHERE instance_id = ?2 AND seq = ?3",
            params![observed(n + 1), straddled, (n + 1) as i64],
        )
        .unwrap();
    }
    // The live documents were built with the clock's observed_at; rebuild
    // them from the normalised rows by replaying the same path on a copy.
    db.execute_batch("DELETE FROM journal_search_docs; DELETE FROM journal_search_rows;")
        .unwrap();
    for (n, event) in rows.iter().enumerate() {
        index_event(&db, &live, (n + 1) as i64, event, &observed(n + 1)).unwrap();
    }
    for (n, event) in rows.iter().enumerate().skip(half) {
        index_event(&db, &straddled, (n + 1) as i64, event, &observed(n + 1)).unwrap();
    }
    drop(db);
    let mut db = fx.db();
    loop {
        if backfill_batch(&mut db, 4).unwrap().done {
            break;
        }
    }
    let want = fx.docs_of(&live);
    assert!(want.len() >= 8, "{want:?}");
    assert_eq!(fx.docs_of(&backfilled), want);
    assert_eq!(fx.docs_of(&straddled), want);
    // And the documents say what the fold rules say.
    let texts: Vec<&str> = want.iter().map(|doc| doc.8.as_str()).collect();
    for text in [
        "please revert migration 0042",
        "please revert migration 0099 now",
        "I'll check the rollback",
        "Bash git log --oneline",
        "abc123 revert 0042",
        "Done with the ledger",
        "hub note one",
        "hub note two",
    ] {
        assert!(texts.contains(&text), "{text}: {texts:?}");
    }
    assert_eq!(texts.len(), 8, "{texts:?}");
}

fn complete(mut event: Value) -> Value {
    event["payload"]["status"] = json!("complete");
    event
}

/// A PTY-shaped stream past the every-row threshold: open, 20 deltas of
/// ~500 bytes, then a short final delta with a unique word.
fn long_stream(node: &str, last: Value) -> Vec<Value> {
    let mut rows = vec![streamed(node, "open", "Start ")];
    for n in 0..20 {
        rows.push(streamed(
            node,
            "append",
            &format!("chunk{n} {} ", "filler ".repeat(70)),
        ));
    }
    rows.push(last);
    rows
}

/// r3 item 1: a long PTY answer ends with an `append` and no close; its
/// final delta (under the 1 KiB re-render step) is in the indexed text, live
/// and through the backfill.
#[tokio::test]
async fn the_final_delta_of_a_long_stream_is_findable_live_and_backfilled() {
    let fx = fixture().await;
    let rows = long_stream("m1", complete(streamed("m1", "append", "zebrafinch end.")));
    let live = fx.instance().await;
    for event in &rows {
        fx.append(&live, event.clone()).await;
    }
    let backfilled = fx.instance().await;
    let db = fx.db();
    for (n, event) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, '2026-11-01T00:00:00.000Z')",
            params![
                backfilled,
                (n + 1) as i64,
                format!("evt-{n}"),
                event.to_string()
            ],
        )
        .unwrap();
    }
    drop(db);
    let mut db = fx.db();
    while !backfill_batch(&mut db, 7).unwrap().done {}
    let found = fx.search_limit(human(), "zebrafinch", 10).await;
    let mut ids: Vec<String> = found
        .hits
        .iter()
        .map(|hit| hit.instance_id.clone())
        .collect();
    ids.sort();
    let mut want = vec![live.clone(), backfilled.clone()];
    want.sort();
    assert_eq!(ids, want);
    for instance in [&live, &backfilled] {
        let (raw, text, pending): (String, String, bool) = db
            .query_row(
                "SELECT raw, text, pending FROM journal_search_docs WHERE instance_id = ?1",
                [instance],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(raw.len() > 8 * 1024, "{}", raw.len());
        assert!(
            text.ends_with("zebrafinch end."),
            "{}",
            &text[text.len() - 40..]
        );
        assert!(!pending);
    }
}

/// A stream whose carrier never marks its end converges too: the settle
/// sweep renders a lagging document once it has been quiet.
#[tokio::test]
async fn an_unmarked_long_stream_is_settled_once_quiet() {
    let fx = fixture().await;
    let ins = fx.instance().await;
    for event in long_stream("m1", streamed("m1", "append", "kestrelwing end.")) {
        fx.append(&ins, event).await;
    }
    // Between renders the tail is in raw only.
    assert!(fx.search(human(), "kestrelwing").await.hits.is_empty());
    // Not yet quiet: nothing to settle.
    assert_eq!(
        fx.store
            .journal_search_settle(Duration::from_secs(3600))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        fx.store
            .journal_search_settle(Duration::ZERO)
            .await
            .unwrap(),
        1
    );
    assert_eq!(fx.search(human(), "kestrelwing").await.hits.len(), 1);
    assert_eq!(
        fx.store
            .journal_search_settle(Duration::ZERO)
            .await
            .unwrap(),
        0
    );
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn file_bytes(dir: &Path) -> u64 {
    ["hub.sqlite", "hub.sqlite-wal"]
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok())
        .map(|meta| meta.len())
        .sum()
}

/// Measurement, not a gate (cb-search): point `REMUDA_SEARCH_BENCH_DIR` at a
/// directory holding a COPY of a large `hub.sqlite` below the search rung.
/// The test migrates it, backfills through the writer as the Hub does,
/// then times queries and appends, and prints the numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; needs REMUDA_SEARCH_BENCH_DIR"]
async fn bench_backfill_query_and_append_on_a_large_journal() {
    let Some(dir) = std::env::var_os("REMUDA_SEARCH_BENCH_DIR").map(PathBuf::from) else {
        return;
    };
    let before = file_bytes(&dir);
    let opened = Instant::now();
    let store = Store::open(&dir).unwrap();
    println!("open+migrate ms={}", opened.elapsed().as_millis());
    let status = store.journal_search_status(true).await.unwrap();
    println!(
        "before: bytes={before} journal_rows={:?} docs={:?} state={:?}",
        status.journal_rows, status.docs, status.state
    );
    // Backfill, timed per writer job.
    let started = Instant::now();
    let mut batch_ms = Vec::new();
    let (mut scanned, mut indexed) = (0usize, 0usize);
    loop {
        let t = Instant::now();
        let step = store
            .journal_search_backfill_batch(BACKFILL_BATCH_ROWS)
            .await
            .unwrap();
        batch_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        scanned += step.scanned;
        indexed += step.indexed;
        if step.done {
            break;
        }
    }
    let secs = started.elapsed().as_secs_f64();
    batch_ms.sort_by(f64::total_cmp);
    println!(
        "backfill: rows={scanned} indexed={indexed} batches={} secs={secs:.1} rows_per_sec={:.0} \
         batch_ms p50={:.1} p95={:.1} max={:.1}",
        batch_ms.len(),
        scanned as f64 / secs,
        percentile(&batch_ms, 50.0),
        percentile(&batch_ms, 95.0),
        batch_ms.last().copied().unwrap_or(0.0)
    );
    let db = Connection::open(dir.join("hub.sqlite")).unwrap();
    db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let after = file_bytes(&dir);
    println!(
        "growth: before={before} after={after} delta={} ({:.1}%)",
        after - before,
        (after - before) as f64 * 100.0 / before as f64
    );
    // Queries: the owner's world and one seat's lineage.
    let (seat, host): (String, String) = db
        .query_row(
            "SELECT id, host_id FROM instances
              WHERE COALESCE(lineage_id, id) = id
                AND json_extract(spec_json, '$.parentInstanceId') IS NULL
              ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    for query in [
        "journal",
        "supervisor restart",
        "fence generation lineage",
        "quartermaster",
        "迁移",
        "id",
        "边界 条件",
    ] {
        for (who, scope) in [("human", human()), ("agent", agent(&seat))] {
            let mut ms = Vec::new();
            let mut hits = 0;
            let mut partial = false;
            for _ in 0..15 {
                let t = Instant::now();
                let result = store
                    .journal_search(scope.clone(), query.to_string(), 20, QUERY_BUDGET)
                    .await
                    .unwrap();
                ms.push(t.elapsed().as_secs_f64() * 1000.0);
                hits = result.hits.len();
                partial |= result.partial;
            }
            ms.sort_by(f64::total_cmp);
            println!(
                "query {who:5} {query:28} hits={hits:2} partial={partial} p50_ms={:.1} p95_ms={:.1}",
                percentile(&ms, 50.0),
                percentile(&ms, 95.0)
            );
        }
    }
    // Append overhead: the same path and payload size, indexed (a message's
    // complete form) versus not (a thought).
    let text = "The supervisor restarts the chapter when the lineage fence moves; \
                migration 0042 rolls back the journal index ";
    let mut per_kind = Vec::new();
    for kind in ["thought", "message", "thought", "message"] {
        let t = Instant::now();
        for n in 0..1000 {
            let mut event = message(
                &format!("bench-{kind}-{n}-{}", per_kind.len()),
                "close",
                "assistant",
                text,
            );
            event["kind"] = json!(kind);
            store
                .append_journal(host.clone(), seat.clone(), None, event)
                .await
                .unwrap();
        }
        per_kind.push((kind, t.elapsed().as_secs_f64() * 1000.0 / 1000.0));
    }
    for (kind, ms) in &per_kind {
        println!("append {kind:8} ms_per_event={ms:.3}");
    }
    // A streamed reply: open, then token-sized deltas into one node, past
    // the 8 KiB every-row threshold (about 24 KiB in all).
    let words = [
        "supervisor ",
        "restart ",
        "chapter ",
        "lineage ",
        "fence ",
        "ledger ",
    ];
    for (label, rows) in [("stream-thought", "thought"), ("stream-message", "message")] {
        let node = format!("bench-{label}");
        let t = Instant::now();
        let mut first = streamed(&node, "open", "Start ");
        first["kind"] = json!(rows);
        store
            .append_journal(host.clone(), seat.clone(), None, first)
            .await
            .unwrap();
        for n in 0..3000 {
            let mut event = streamed(&node, "append", words[n % words.len()]);
            event["kind"] = json!(rows);
            store
                .append_journal(host.clone(), seat.clone(), None, event)
                .await
                .unwrap();
        }
        println!(
            "append {label} ms_per_delta={:.3}",
            t.elapsed().as_secs_f64() * 1000.0 / 3001.0
        );
    }
}
