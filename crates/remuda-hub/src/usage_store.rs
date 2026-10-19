//! Hub-side consumer for `UsagePayload` journal events; coordinator §4.5.
//!
//! The Node-side usage adapters (P6, `remuda-driver/src/usage/*`) extract
//! token/cost frames; this module only **consumes** them at the Hub: every
//! `kind:"usage"` journal append is persisted once into `usage_events` and
//! aggregated per `(providerProfile, model, window)`. The aggregates backfill
//! `windows[].source = "inferred"` and reconcile against task budgets.
//!
//! Money is always an estimate (the payload's `accounting` says so) and the
//! hard budget band is ×1.15 per design §4.5/§7 risk 4.

use crate::AppState;
use crate::store::JournalRecord;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BUDGET_STOP_FACTOR: f64 = 1.15;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEventRow {
    pub instance_id: String,
    pub seq: i64,
    pub profile_id: Option<String>,
    pub model: Option<String>,
    pub scope: String,
    pub scope_id: Option<String>,
    pub turn_id: Option<String>,
    pub metric_revision: i64,
    pub source_session_id: Option<String>,
    pub mode: String,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cost_usd: Option<String>,
    pub accounting: String,
    pub observed_at: String,
    pub observed_at_source: String,
    pub native_context_window: Option<i64>,
}

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    run_migrations(&tx)?;
    tx.commit()
}

const META_DDL: &str = "CREATE TABLE IF NOT EXISTS usage_schema_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
     );";

const MARKER_SCOPE_CONSOLIDATION: &str = "scope_consolidation_v1";
const MARKER_JOURNAL_REPLAY: &str = "journal_replay_v1";

#[cfg(any(test, feature = "test-faults"))]
const TEST_FORCE_FAILURE_TABLE: &str = "remuda_test_force_usage_migration_failure";

#[cfg(any(test, feature = "test-faults"))]
fn test_failure_forced(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master
         WHERE type = 'table' AND name = ?1",
        [TEST_FORCE_FAILURE_TABLE],
        |row| row.get(0),
    )
}

fn set_meta_marker(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO usage_schema_meta (key, value) VALUES (?1, 'applied')",
        [key],
    )?;
    Ok(())
}

const USAGE_REQUIRED_COLUMNS: &[&str] = &[
    "cache_read_tokens",
    "cache_write_tokens",
    "scope_id",
    "turn_id",
    "metric_revision",
    "observed_at_source",
    "native_context_window",
    "source_session_id",
    "lineage_state",
];

pub(crate) fn usage_schema_is_current(conn: &Connection) -> rusqlite::Result<bool> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master
         WHERE type = 'table' AND name = 'usage_events'",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(false);
    }
    for column in USAGE_REQUIRED_COLUMNS {
        if !crate::store::column_exists(conn, "usage_events", column)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_events (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            profile_id TEXT,
            model TEXT,
            scope TEXT NOT NULL,
            scope_id TEXT,
            turn_id TEXT,
            mode TEXT NOT NULL,
            total_tokens INTEGER,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            cost_usd TEXT,
            accounting TEXT NOT NULL DEFAULT 'estimated',
            observed_at TEXT NOT NULL,
            observed_at_source TEXT NOT NULL DEFAULT 'ingest',
            native_context_window INTEGER,
            metric_revision INTEGER NOT NULL DEFAULT 1,
            source_session_id TEXT,
            PRIMARY KEY (instance_id, seq)
         );
         CREATE INDEX IF NOT EXISTS usage_events_profile_model
             ON usage_events(profile_id, model);
         CREATE INDEX IF NOT EXISTS usage_events_instance
             ON usage_events(instance_id);",
    )?;
    // Rows created before the context rollup carried no cache counters.
    crate::store::ensure_column(conn, "usage_events", "cache_read_tokens", "INTEGER")?;
    crate::store::ensure_column(conn, "usage_events", "cache_write_tokens", "INTEGER")?;
    // Context-window rollup (c-ctxusage RC1): durable per-turn dedupe key.
    // MUST be added before the partial unique index that references it.
    crate::store::ensure_column(conn, "usage_events", "scope_id", "TEXT")?;
    // c-usagefu r6 item 5: native turn id for codex Message rows, so the
    // rollup can count distinct turns after the Turn emission was removed.
    crate::store::ensure_column(conn, "usage_events", "turn_id", "TEXT")?;
    // r2: snapshot revision for revision-aware scoped upserts. Added as a
    // nullable column for upgrade compatibility, then backfilled: rows written
    // at the f890028e schema got NULL here, and a NULL on the right-hand side of
    // `excluded.revision > usage_events.revision` is never true, so those
    // sessions would freeze forever (c-ctxusage r3 item 3). Pin them at 1 —
    // their original (only) revision — so a genuinely newer snapshot replaces.
    crate::store::ensure_column(conn, "usage_events", "metric_revision", "INTEGER")?;
    conn.execute(
        "UPDATE usage_events SET metric_revision = 1 WHERE metric_revision IS NULL",
        [],
    )?;
    // c-ctxusage r15 item 4: the turn watermark is session-qualified
    // (native session id + record position). Existing rows are the
    // UNQUALIFIED legacy lineage: NULL is the defined migration value, kept
    // distinct from a reported empty/unknown session, and the comparison rule
    // only applies a numeric watermark within one proven lineage.
    crate::store::ensure_column(conn, "usage_events", "source_session_id", "TEXT")?;
    // c-ctxusage r4 item 5: remember whether observed_at is native-sourced or
    // an ingest fallback, so an unchanged-counters replay can repair the
    // historical native time.
    //
    // r37 item 3: the DEFAULT stamped onto column-less rows depends on the
    // shape being upgraded. A database from before this column carried NO
    // provenance at all, so its retained timestamp is the only evidence and
    // gets 'legacy' (the c-usagefu r8 item 5 compatibility trust: upgraded
    // instances still derive rates from their cumulative increments). A
    // database that ALREADY had the column carries real provenance: an
    // explicit 'ingest' is a timestamp-free historical replay whose time is
    // the ingest clock, and the blanket upgrade tag must never promote it to
    // time authority — so NULL/empty rows there default to 'ingest'.
    let had_observed_at_source =
        crate::store::column_exists(conn, "usage_events", "observed_at_source")?;
    crate::store::ensure_column(conn, "usage_events", "observed_at_source", "TEXT")?;
    let default_observed_at_source = if had_observed_at_source {
        "ingest"
    } else {
        "legacy"
    };
    conn.execute(
        "UPDATE usage_events SET observed_at_source = ?1
         WHERE observed_at_source IS NULL OR observed_at_source = ''",
        params![default_observed_at_source],
    )?;
    // c-usagefu (c): native per-model context window reported on the payload.
    crate::store::ensure_column(conn, "usage_events", "native_context_window", "INTEGER")?;
    // c-ctxusage × c-usagefu merge, Ruling AE3: lineage_state is PRODUCER-ERA
    // provenance, not a recovery marker. Pre-upgrade producers and the current
    // codex adapter emit identically shaped turns with different billing
    // semantics (r29 conservative ambiguity vs Ruling B flow); the producer era
    // is the only discriminator. The era boundary per instance is the cutover:
    // the migration that INTRODUCES this column. Every row that exists at the
    // cutover is 'legacy' — including rows with no journal frame, since they too
    // were produced by the old era. The cutover seq is MAX(journal.seq) at the
    // cutover moment; replay ingests a frame with seq <= cutover as legacy and
    // any later frame as live, via an explicit per-frame provenance input —
    // never a post-hoc blanket UPDATE. A later restart sees the column already
    // present and post-upgrade live rows (NULL) stay live.
    let tag_legacy_rows = !crate::store::column_exists(conn, "usage_events", "lineage_state")?;
    crate::store::ensure_column(conn, "usage_events", "lineage_state", "TEXT")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_instance_era (
            instance_id TEXT PRIMARY KEY,
            cutover_seq INTEGER NOT NULL
         );",
    )?;
    if tag_legacy_rows {
        conn.execute("UPDATE usage_events SET lineage_state = 'legacy'", [])?;
        // AE3(a): record the cutover for the UNION of every instance present
        // in usage_events or the journal. The boundary is the journal MAX(seq)
        // at the cutover moment; an instance with usage rows but no journal
        // frames (or no journal table at all) gets an explicit boundary of 0.
        // Case-(c) derivation below never overrides these rows (INSERT OR
        // IGNORE), so column-introduction upgrades keep their true boundary.
        let journal_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'journal'",
            [],
            |row| row.get(0),
        )?;
        if journal_exists > 0 {
            conn.execute(
                "INSERT OR IGNORE INTO usage_instance_era(instance_id, cutover_seq)
                 SELECT inst.instance_id, COALESCE(MAX(j.seq), 0)
                 FROM (
                     SELECT DISTINCT instance_id FROM usage_events
                     UNION
                     SELECT DISTINCT instance_id FROM journal
                 ) inst
                 LEFT JOIN journal j ON j.instance_id = inst.instance_id
                 GROUP BY inst.instance_id",
                [],
            )?;
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO usage_instance_era(instance_id, cutover_seq)
                 SELECT DISTINCT instance_id, 0 FROM usage_events",
                [],
            )?;
        }
    }
    conn.execute_batch(META_DDL)?;
    // c-ctxusage r10/r29-r31 + c-usagefu r13-r16 identity recovery: the whole
    // scope/lineage consolidation and the fallback-field reconstruction run in
    // ONE migration transaction so an interruption never leaves the schema or
    // the data half-migrated. The r29-r31 lineage collapse is the superset of
    // usagefu's marker-guarded scope block (it is idempotent itself and covers
    // the already-complete-r14/r28 shapes directly), but the usagefu fallback
    // projection (turn_id / native time / revision / context window for rows
    // written while an earlier migration window was rolled back) still runs.
    // `conn` IS the outer migration transaction opened by `migrate`
    // (Ruling M1: exactly one migration transaction, never nested); alias it
    // so the steps below read as transactional work.
    let tx = conn;
    // c-usagefu r15 item 1 test seam: a forced failure AFTER the additive DDL
    // rolls the columns back too, exercising the legacy-shape fallback.
    #[cfg(any(test, feature = "test-faults"))]
    if test_failure_forced(tx)? {
        tx.execute_batch("SELECT remuda_test_force_usage_migration_failure();")?;
    }
    // c-ctxusage r10 item 2 / r30 item 1: the obsolete shared and per-scope
    // unique indexes MUST be dropped BEFORE journal replay inserts can hit a
    // historical duplicate that the old index forbade.
    tx.execute_batch("DROP INDEX IF EXISTS usage_events_scope_dedupe;")?;
    tx.execute_batch("DROP INDEX IF EXISTS usage_events_turn_dedupe;")?;
    // Ruling AE2 (recovery IS replay, complete and undecorated): for every
    // instance that owns at least one journal-backed usage row, delete ALL of
    // that instance's journal-backed rows and replay EVERY usage frame of its
    // journal in (seq, rowid) order through the live ingest path. Rows without
    // journal backing are never deleted or tagged; the only touch they receive
    // is the live-rule fold required to build the new unique indexes.
    replay_fallback_rows_from_journal(tx)?;
    set_meta_marker(conn, MARKER_JOURNAL_REPLAY)?;
    // r16 item 3 (T1'): recreate the per-LINEAGE turn unique index. When the
    // replay helper ran against a database with a journal table it already
    // created both live invariants; on a journal-less database these are the
    // authoritative creates.
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS usage_events_turn_dedupe
             ON usage_events(
                 instance_id, scope, scope_id,
                 COALESCE(source_session_id, '')
             )
             WHERE scope = 'turn' AND scope_id IS NOT NULL;",
    )?;
    // Durable Message dedupe (live invariant relied on by upsert_message_row).
    tx_batch_create_message_index(tx)?;
    // The outer `migrate` owns the single transaction and commits it.
    // Record completion for usagefu r15's one-time marker (no-op on older paths).
    set_meta_marker(conn, MARKER_SCOPE_CONSOLIDATION)?;
    Ok(())
}

fn tx_batch_create_message_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS usage_events_message_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'message' AND scope_id IS NOT NULL;",
    )
}

/// AE3(c): initialize any MISSING per-instance cutover metadata for a database
/// that already has the lineage column (unreleased intermediate branch build).
/// This is independent of the one-time replay marker and of journal replay
/// running at all, so it must be reached even when the replay guard or the
/// missing-journal table would otherwise return early. Instances upgraded by
/// case (a) already have a row, which INSERT OR IGNORE preserves.
fn ensure_intermediate_era_metadata(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO usage_instance_era(instance_id, cutover_seq)
         SELECT u.instance_id,
                COALESCE(MAX(CASE WHEN u.lineage_state = 'legacy' THEN u.seq END), 0)
         FROM usage_events u
         GROUP BY u.instance_id",
        [],
    )?;
    Ok(())
}

fn replay_fallback_rows_from_journal(conn: &Connection) -> rusqlite::Result<usize> {
    // AE3(c) first: fill cutover metadata for column-present intermediate
    // databases regardless of whether replay runs or the completion marker is
    // already applied. (Reads only usage_events, so it precedes both guards.)
    ensure_intermediate_era_metadata(conn)?;
    // A database without a journal table (unit migrations, a fresh store that
    // never appended observations) has nothing to replay from.
    let journal_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'journal'",
        [],
        |row| row.get(0),
    )?;
    if journal_exists == 0 {
        return Ok(0);
    }
    // The one-time AE replay has already committed on this database.
    let already_replayed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM usage_schema_meta WHERE key = ?1 AND value = 'applied'",
        [MARKER_JOURNAL_REPLAY],
        |row| row.get(0),
    )?;
    if already_replayed > 0 {
        return Ok(0);
    }
    // AE2 (i)/AE3: the unit of replay is the INSTANCE. An instance participates
    // iff at least one of its physical usage rows is backed by a journal frame
    // at the SAME (instance, seq). Backing is physical EXISTENCE in the
    // journal, regardless of the frame kind (a usage_events row's seq always
    // mirrors the usage frame that produced it). Frames are NEVER selected by
    // "a physical row still represents this seq": the live Message upsert keeps
    // the FIRST seq while a revision lands at a later seq, and the live Turn
    // upsert ADVANCES seq to a correction's seq — so surviving physical seqs are
    // exactly not the set of frames to replay.
    let instances: Vec<String> = {
        let mut st = conn.prepare(
            "SELECT DISTINCT j.instance_id
             FROM journal j
             JOIN usage_events u
               ON u.instance_id = j.instance_id AND u.seq = j.seq
             ORDER BY j.instance_id",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // The instance table may legitimately stay empty: the unjournaled fold
    // below still has to run on databases that accumulated duplicates while an
    // earlier migration window was rolled back.
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS r46_replay_instances(value TEXT PRIMARY KEY);
         DELETE FROM r46_replay_instances;",
    )?;
    {
        let mut ins = conn.prepare("INSERT INTO r46_replay_instances(value) VALUES (?1)")?;
        for instance in &instances {
            ins.execute(params![instance])?;
        }
    }

    // Profile/model attribution is durable per-frame provider attribution that
    // the wire payload does not carry. Resolve it from the backed physical
    // rows BEFORE they are deleted, at three granularities:
    //  * exact (instance, seq): a physical row sat at this seq;
    //  * durable key (scope/scope_id/lineage): the surviving row of a collapsed
    //    lineage carries the final attribution earlier frames folded onto;
    //  * instance: deterministic first-backed-row fallback.
    #[derive(Clone, Default)]
    struct Attribution {
        profile_id: Option<String>,
        model: Option<String>,
    }
    let mut exact_attr: std::collections::HashMap<(String, i64), Attribution> =
        std::collections::HashMap::new();
    let mut key_attr: std::collections::HashMap<
        (String, String, String, String),
        (i64, Attribution),
    > = std::collections::HashMap::new();
    let mut inst_attr: std::collections::HashMap<String, (i64, Attribution)> =
        std::collections::HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT j.instance_id, j.seq, u.scope,
                    COALESCE(u.scope_id, ''), COALESCE(u.source_session_id, ''),
                    u.profile_id, u.model
             FROM usage_events u
             JOIN journal j ON j.instance_id = u.instance_id AND j.seq = u.seq
             WHERE j.instance_id IN (SELECT value FROM r46_replay_instances)
             ORDER BY j.instance_id, j.seq, u.rowid",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        for row in rows {
            let (instance, seq, scope, scope_id, sid, profile_id, model) = row?;
            let attr = Attribution { profile_id, model };
            exact_attr
                .entry((instance.clone(), seq))
                .or_insert_with(|| attr.clone());
            // Key attribution prefers the HIGHEST surviving seq: the live Turn
            // upsert advances seq onto the surviving row, while the Message
            // upsert keeps a single survivor at the first seq (message
            // attribution never changes under fold, so either row agrees).
            let key = (instance.clone(), scope, scope_id, sid);
            match key_attr.get(&key) {
                Some((kept_seq, _)) if *kept_seq >= seq => {}
                _ => {
                    key_attr.insert(key, (seq, attr.clone()));
                }
            }
            match inst_attr.get(&instance) {
                Some((kept_seq, _)) if *kept_seq <= seq => {}
                _ => {
                    inst_attr.insert(instance, (seq, attr));
                }
            }
        }
    }

    // AE2 (i): delete ALL journal-backed usage rows of the participating
    // instances — every kind (Session/Turn/Message) — backed by PHYSICAL
    // (instance, seq) existence in the journal. The journal is untouched, so a
    // failure inside this transaction rolls every delete back.
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS r46_backed_rowids(value INTEGER NOT NULL);
         DELETE FROM r46_backed_rowids;",
    )?;
    conn.execute(
        "INSERT INTO r46_backed_rowids(value)
         SELECT u.rowid
         FROM usage_events u
         JOIN journal j ON j.instance_id = u.instance_id AND j.seq = u.seq
         WHERE j.instance_id IN (SELECT value FROM r46_replay_instances)",
        [],
    )?;
    conn.execute(
        "DELETE FROM usage_events WHERE rowid IN (SELECT value FROM r46_backed_rowids)",
        [],
    )?;

    // AE2 (ii) exception: now that journal-backed rows are gone, capture the
    // physical duplicates that remain on the two NEW unique keys — they are
    // necessarily UNJOURNALED (backed rows in participating instances were
    // just removed; non-participating instances never had backed rows). The
    // survivor (MIN rowid) stays; later copies are removed so the indexes can
    // build, then re-fed through the SAME live upsert rule after the indexes
    // exist. Running this only AFTER the backed delete is what keeps a mixed
    // collision group (backed survivor beside an unjournaled later revision)
    // from folding the unjournaled revision away before the backed rows are
    // rebuilt. The fold runs on every migration, including one with no replay.
    let mut captured_folds = fold_unjournaled_index_collisions(conn)?;
    // AE3 medium-2 regression gate: after folding, no exact-key collision may
    // remain or the unique-index create would fail (test builds only).
    #[cfg(test)]
    assert_no_index_collisions(conn);
    tx_batch_create_turn_index(conn)?;
    tx_batch_create_message_index(conn)?;
    captured_folds.sort_by_key(|(rowid, _, _)| *rowid);
    for (_, row, era) in &captured_folds {
        // Re-feed with the captured row's OWN producer era — folding preserves
        // provenance, never reclassifies it.
        insert_usage_event_era(conn, row, *era)?;
    }
    if instances.is_empty() {
        return Ok(0);
    }

    // AE2 (i): replay EVERY usage frame of each participating instance in
    // journal order, through the exact projector + insert the live ingest path
    // uses.
    let records: Vec<JournalRecord> = {
        let mut st = conn.prepare(
            "SELECT j.instance_id, j.seq, j.event_id, j.payload_json, j.observed_at
             FROM journal j
             WHERE json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND j.instance_id IN (SELECT value FROM r46_replay_instances)
             ORDER BY j.instance_id, j.seq, j.rowid",
        )?;
        let rows = st.query_map([], |r| -> rusqlite::Result<JournalRecord> {
            let payload: String = r.get(3)?;
            let event = serde_json::from_str(&payload).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                )
            })?;
            Ok(JournalRecord {
                instance_id: r.get(0)?,
                seq: r.get(1)?,
                event_id: r.get(2)?,
                event,
                observed_at: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let lookup_attr = |record: &JournalRecord, row: &UsageEventRow| -> Option<&Attribution> {
        exact_attr
            .get(&(record.instance_id.clone(), record.seq))
            .or_else(|| {
                key_attr
                    .get(&(
                        record.instance_id.clone(),
                        row.scope.clone(),
                        row.scope_id.clone().unwrap_or_default(),
                        row.source_session_id.clone().unwrap_or_default(),
                    ))
                    .map(|(_, a)| a)
            })
            .or_else(|| inst_attr.get(&record.instance_id).map(|(_, a)| a))
    };
    // AE3(b): per-instance producer-era boundary. A frame with seq <= the
    // cutover seq is ingested with legacy provenance; a later frame is live.
    let mut cutover: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT instance_id, cutover_seq FROM usage_instance_era
             WHERE instance_id IN (SELECT value FROM r46_replay_instances)",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (instance, seq) = row?;
            cutover.insert(instance, seq);
        }
    }
    let mut rebuilt = 0usize;
    for record in &records {
        // Project once to learn the durable key the live path folds on, then
        // resolve attribution the surviving physical row carried.
        let Some(pre) = project_usage_event(record, None, None) else {
            continue;
        };
        let default_attr = Attribution::default();
        let attr = lookup_attr(record, &pre).unwrap_or(&default_attr);
        let Some(row) =
            project_usage_event(record, attr.profile_id.as_deref(), attr.model.as_deref())
        else {
            continue;
        };
        // The frame carries its producer era explicitly — no post-hoc tag.
        let era = if record.seq <= *cutover.get(&record.instance_id).unwrap_or(&0) {
            IngestEra::Legacy
        } else {
            IngestEra::Live
        };
        insert_usage_event_era(conn, &row, era)?;
        rebuilt += 1;
    }
    // AE3: replayed rows carry per-frame producer era, column for column with
    // live ingest — no blanket recovery marking.
    Ok(rebuilt)
}

/// Create the per-lineage Turn unique index (live invariant).
fn tx_batch_create_turn_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS usage_events_turn_dedupe
             ON usage_events(
                 instance_id, scope, scope_id,
                 COALESCE(source_session_id, '')
             )
             WHERE scope = 'turn' AND scope_id IS NOT NULL;",
    )
}

/// Test-only gate run between the unjournaled fold and the unique-index
/// creates: each live unique key must already be collision-free.
#[cfg(test)]
fn assert_no_index_collisions(conn: &Connection) {
    let dupes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (
                SELECT 1 FROM usage_events
                 WHERE scope_id IS NOT NULL AND scope IN ('message', 'turn')
                 GROUP BY scope, instance_id, scope_id,
                          CASE WHEN scope = 'turn'
                               THEN COALESCE(source_session_id, '')
                               ELSE '' END
                HAVING COUNT(*) > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dupes, 0,
        "a unique-key collision remains after the unjournaled fold"
    );
}

/// Physical rows that collide on a unique index the current live schema needs,
/// among rows the replay does not rebuild (rows without a journal frame at
/// their seq). The SURVIVOR (MIN rowid per the EXACT index key) stays in place;
/// every later physical row is captured and removed, then returned so the
/// caller can create the unique indexes and re-feed the captures through the
/// live upsert rule — higher revision wins, omitted metadata retained, never a
/// discard-to-MIN(rowid) sweep. Grouping matches each index's key columns
/// exactly: the Message index ignores source_session_id; the Turn index keeps
/// the lineage qualifier.
fn fold_unjournaled_index_collisions(
    conn: &Connection,
) -> rusqlite::Result<Vec<(i64, UsageEventRow, IngestEra)>> {
    struct Fold {
        scope: &'static str,
        // Correlated predicate: true for rows that are NOT the MIN(rowid)
        // survivor of their own exact unique-key group (outer row is `u`).
        non_survivor: &'static str,
    }
    let folds = [
        Fold {
            scope: "message",
            // Message key is (instance_id, scope, scope_id) — source_session_id
            // is deliberately NOT part of the key.
            non_survivor: "u.rowid > (
                SELECT MIN(g.rowid) FROM usage_events g
                 WHERE g.instance_id = u.instance_id
                   AND g.scope = u.scope
                   AND g.scope_id = u.scope_id
            )",
        },
        Fold {
            scope: "turn",
            // Turn key additionally carries the lineage qualifier.
            non_survivor: "u.rowid > (
                SELECT MIN(g.rowid) FROM usage_events g
                 WHERE g.instance_id = u.instance_id
                   AND g.scope = u.scope
                   AND g.scope_id = u.scope_id
                   AND COALESCE(g.source_session_id, '') =
                       COALESCE(u.source_session_id, '')
            )",
        },
    ];

    let mut captured: Vec<(i64, UsageEventRow, IngestEra)> = Vec::new();
    for fold in &folds {
        let sql = format!(
            "SELECT rowid, instance_id, seq, profile_id, model, scope, scope_id, turn_id,
                    metric_revision, source_session_id, mode,
                    total_tokens, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                    observed_at, observed_at_source, native_context_window,
                    lineage_state
             FROM usage_events u
             WHERE u.scope = ?1 AND u.scope_id IS NOT NULL
               AND {non_survivor}
             ORDER BY u.rowid",
            non_survivor = fold.non_survivor
        );
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(params![fold.scope], |r| {
            let rowid: i64 = r.get(0)?;
            let lineage: Option<String> = r.get(21)?;
            let era = if lineage.as_deref() == Some("legacy") {
                IngestEra::Legacy
            } else {
                IngestEra::Live
            };
            let row = UsageEventRow {
                instance_id: r.get(1)?,
                seq: r.get(2)?,
                profile_id: r.get(3)?,
                model: r.get(4)?,
                scope: r.get(5)?,
                scope_id: r.get(6)?,
                turn_id: r.get(7)?,
                metric_revision: r.get(8)?,
                source_session_id: r.get(9)?,
                mode: r.get(10)?,
                total_tokens: r.get(11)?,
                input_tokens: r.get(12)?,
                output_tokens: r.get(13)?,
                cache_read_tokens: r.get(14)?,
                cache_write_tokens: r.get(15)?,
                cost_usd: r.get(16)?,
                accounting: r.get(17)?,
                observed_at: r.get(18)?,
                observed_at_source: r.get(19)?,
                native_context_window: r.get(20)?,
            };
            Ok((rowid, row, era))
        })?;
        for item in rows {
            captured.push(item?);
        }
    }
    if captured.is_empty() {
        return Ok(Vec::new());
    }
    // Remove the captured physical copies so the unique indexes can build.
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS r46_fold_rowids(value INTEGER PRIMARY KEY);
         DELETE FROM r46_fold_rowids;",
    )?;
    {
        let mut ins = conn.prepare("INSERT INTO r46_fold_rowids(value) VALUES (?1)")?;
        for (rowid, _, _) in &captured {
            ins.execute(params![rowid])?;
        }
    }
    conn.execute(
        "DELETE FROM usage_events WHERE rowid IN (SELECT value FROM r46_fold_rowids)",
        [],
    )?;
    // Already in global rowid order (ORDER BY u.rowid per scope); the caller
    // creates the unique indexes and re-feeds these so INSERT OR IGNORE hits
    // the exact-key survivor and the live revision/metadata rule folds content.
    Ok(captured)
}

#[derive(Clone, Debug)]
struct TurnFrame {
    rowid: Option<i64>,
    seq: i64,
    profile_id: Option<String>,
    model: Option<String>,
    mode: String,
    revision: i64,
    source_session_id: Option<String>,
    counters: SnapshotCounters,
    cost_usd: Option<String>,
    accounting: String,
    observed_at: String,
    observed_at_source: String,
    native_context_window: Option<i64>,
    // Producer era of THIS frame. A persisted row takes the era of whatever
    // authoritative frame is currently accepted (AE3 clarification): a higher
    // accepted revision overwrites it; a rejected revision leaves it stored.
    era: IngestEra,
}

fn reconcile_turn_frame(stored: &TurnFrame, incoming: &TurnFrame) -> TurnFrame {
    let reports_counters = incoming
        .counters
        .token_baskets()
        .iter()
        .any(Option::is_some);
    // Cost-only channel defaults: keep every stored attribution except the
    // advanced position/lineage and the COALESCE bill.
    let mut reconciled = TurnFrame {
        rowid: incoming.rowid.or(stored.rowid),
        seq: incoming.seq,
        profile_id: stored.profile_id.clone(),
        model: stored.model.clone(),
        mode: stored.mode.clone(),
        revision: incoming.revision,
        source_session_id: incoming.source_session_id.clone(),
        counters: stored.counters,
        cost_usd: incoming
            .cost_usd
            .clone()
            .or_else(|| stored.cost_usd.clone()),
        accounting: stored.accounting.clone(),
        observed_at: stored.observed_at.clone(),
        observed_at_source: stored.observed_at_source.clone(),
        // A billing correction never re-reports the context window: keep the
        // retained value (it may be a 1M window the correction omitted).
        native_context_window: stored.native_context_window,
        // The authoritative revision is the incoming one, so its producer era
        // becomes the row's provenance (an accepted higher revision only; a
        // rejected/equal frame never reaches this reconcile).
        era: incoming.era,
    };
    if !reports_counters {
        return reconciled;
    }
    // Counter channel: merge the incoming authoritative baskets over the
    // retained gaps, take the newer attribution, move the clock under the
    // native-trust rule.
    reconciled.counters = incoming.counters.merged_over(&stored.counters);
    reconciled.profile_id = incoming.profile_id.clone();
    reconciled.model = incoming.model.clone();
    reconciled.mode = incoming.mode.clone();
    reconciled.accounting = incoming.accounting.clone();
    // Window follows the same unreported-keeps-retained rule as the baskets.
    reconciled.native_context_window = incoming
        .native_context_window
        .or(stored.native_context_window);
    if incoming.observed_at_source == "native" || stored.observed_at_source != "native" {
        reconciled.observed_at = incoming.observed_at.clone();
        reconciled.observed_at_source = incoming.observed_at_source.clone();
    }
    reconciled
}

fn persist_reconciled_turn(conn: &Connection, frame: &TurnFrame) -> rusqlite::Result<()> {
    let rowid = frame
        .rowid
        .expect("a reconciled turn frame always targets a persisted row");
    conn.execute(
        "UPDATE usage_events SET
            seq = ?1,
            profile_id = ?2,
            model = ?3,
            mode = ?4,
            metric_revision = ?5,
            source_session_id = ?6,
            total_tokens = ?7,
            input_tokens = ?8,
            output_tokens = ?9,
            cache_read_tokens = ?10,
            cache_write_tokens = ?11,
            cost_usd = ?12,
            accounting = ?13,
            observed_at = ?14,
            observed_at_source = ?15,
            native_context_window = ?16,
            lineage_state = ?17
         WHERE rowid = ?18",
        params![
            frame.seq,
            frame.profile_id,
            frame.model,
            frame.mode,
            frame.revision,
            frame.source_session_id,
            frame.counters.total,
            frame.counters.input,
            frame.counters.output,
            frame.counters.cache_read,
            frame.counters.cache_write,
            frame.cost_usd,
            frame.accounting,
            frame.observed_at,
            frame.observed_at_source,
            frame.native_context_window,
            frame.era.lineage_value(),
            rowid,
        ],
    )?;
    Ok(())
}

/// Producer era of an ingested frame (Ruling AE3). `lineage_state` records
/// which producer era emitted a frame: a pre-cutover producer's turn takes the
/// r29 conservative-ambiguity billing rule; a post-cutover live frame takes
/// Ruling B flow. The value is carried explicitly per frame through the insert
/// path — it is never applied by a blanket post-hoc UPDATE.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IngestEra {
    /// Emitted after the lineage cutover: lineage_state stays NULL.
    Live,
    /// Emitted by a pre-cutover producer (seq <= the instance cutover seq):
    /// lineage_state = 'legacy'.
    Legacy,
}

impl IngestEra {
    fn lineage_value(self) -> Option<&'static str> {
        match self {
            IngestEra::Live => None,
            IngestEra::Legacy => Some("legacy"),
        }
    }
}

/// Live ingestion entry point: post-cutover frames always carry the live era.
pub fn insert_usage_event(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    insert_usage_event_era(conn, row, IngestEra::Live)
}

/// Insert a frame with an explicit producer-era provenance (AE3). Journal
/// replay uses this to give a pre-cutover frame its `legacy` lineage and a
/// post-cutover frame the live NULL, column for column with live ingest.
pub fn insert_usage_event_era(
    conn: &Connection,
    row: &UsageEventRow,
    era: IngestEra,
) -> rusqlite::Result<bool> {
    // c-usagefu r15 item 1: on a database whose usage migration never
    // committed (the legacy origin/main shape), record the event through
    // main's plain insert path so usage keeps landing durably — a later
    // successful migration's journal backfill recovers the scope ids. The
    // scoped upsert paths reference columns the legacy table does not have.
    if !usage_schema_is_current(conn)? {
        return insert_legacy_row(conn, row);
    }
    match (row.scope.as_str(), row.scope_id.as_deref()) {
        ("turn", Some(_)) => upsert_turn_snapshot(conn, row, era),
        ("session", Some(_)) => insert_session_growth_point(conn, row, era),
        ("session", None) => insert_legacy_session_point(conn, row, era),
        _ => insert_row(conn, row, era),
    }
}

fn insert_legacy_row(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, mode, total_tokens,
             input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
             cost_usd, accounting, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            row.instance_id,
            row.seq,
            row.profile_id,
            row.model,
            row.scope,
            row.mode,
            row.total_tokens,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.cost_usd,
            row.accounting,
            row.observed_at,
        ],
    )?;
    Ok(inserted > 0)
}

fn existing_session_floors(
    conn: &Connection,
    row: &UsageEventRow,
) -> rusqlite::Result<Vec<SessionStockPoint>> {
    let mut stmt = conn.prepare(
        "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                cost_usd
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session'
           AND scope_id = ?2",
    )?;
    let floors = stmt
        .query_map(params![row.instance_id, row.scope_id], |r| {
            Ok(SessionStockPoint {
                counters: SnapshotCounters {
                    total: r.get(0)?,
                    input: r.get(1)?,
                    output: r.get(2)?,
                    cache_read: r.get(3)?,
                    cache_write: r.get(4)?,
                },
                cost: parse_cost(r.get::<_, Option<String>>(5)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(floors)
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct SessionStockPoint {
    counters: SnapshotCounters,
    cost: Option<f64>,
}

impl SessionStockPoint {
    fn of(row: &UsageEventRow) -> Self {
        Self {
            counters: SnapshotCounters::of(row),
            cost: parse_cost(row.cost_usd.clone()),
        }
    }

    fn eq_stock(&self, other: &Self) -> bool {
        self.counters == other.counters && self.cost == other.cost
    }

    fn tokens_grew_against(&self, stored: &Self) -> bool {
        self.counters.growth_against(&stored.counters).0
    }

    fn cost_decrease_blocks(&self, stored: &Self) -> bool {
        let cost_went_down = matches!(
            (self.cost, stored.cost),
            (Some(incoming), Some(previous)) if incoming < previous
        );
        cost_went_down && !self.tokens_grew_against(stored)
    }
}

fn parse_cost(cost: Option<String>) -> Option<f64> {
    cost.and_then(|raw| raw.parse::<f64>().ok())
}

fn freezes_against(incoming: &SessionStockPoint, existing: &[SessionStockPoint]) -> bool {
    for stored in existing {
        if incoming.eq_stock(stored) {
            return true;
        }
        let (token_grew, token_decreased) = incoming.counters.growth_against(&stored.counters);
        if token_decreased {
            return true;
        }
        // Cost monotonicity is enforced only for the result channel
        // (token-less cumulative points); on a token-growing point the cost is
        // a driver estimate that may reset (r4 item 4).
        if !token_grew && incoming.cost_decrease_blocks(stored) {
            return true;
        }
    }
    false
}

enum SessionPointDecision {
    Freeze,
    Accept { strip_reset_cost: bool },
}

fn decide_session_point(
    incoming: &SessionStockPoint,
    floors: &[SessionStockPoint],
) -> SessionPointDecision {
    if freezes_against(incoming, floors) {
        return SessionPointDecision::Freeze;
    }
    let tokens_grew = floors
        .iter()
        .any(|stored| incoming.tokens_grew_against(stored));
    let cost_reset = floors
        .iter()
        .any(|stored| matches!((incoming.cost, stored.cost), (Some(new), Some(old)) if new < old));
    SessionPointDecision::Accept {
        strip_reset_cost: tokens_grew && cost_reset,
    }
}

fn accept_session_point(
    row: &UsageEventRow,
    floors: &[SessionStockPoint],
) -> Option<UsageEventRow> {
    let incoming = SessionStockPoint::of(row);
    match decide_session_point(&incoming, floors) {
        SessionPointDecision::Freeze => None,
        SessionPointDecision::Accept { strip_reset_cost } => {
            let mut row = row.clone();
            if strip_reset_cost {
                // Token growth with a lower cost is a driver-priced estimate
                // reset (unpriced turn → priced turn): keep the real token
                // counters, persist the cost as Unknown.
                row.cost_usd = None;
                // r43 AC4 (restart == live): stripping can make the point
                // content-identical to an already-accepted one (a bucket that
                // merely went Unknown->0 while the stale cost disappears).
                // Migration freezes such a point, so live must freeze it too
                // instead of appending a duplicate restart would later delete.
                let stripped = SessionStockPoint::of(&row);
                if freezes_against(&stripped, floors) {
                    return None;
                }
            }
            Some(row)
        }
    }
}

fn insert_session_growth_point(
    conn: &Connection,
    row: &UsageEventRow,
    era: IngestEra,
) -> rusqlite::Result<bool> {
    let floors = existing_session_floors(conn, row)?;
    let Some(row) = accept_session_point(row, &floors) else {
        return Ok(false);
    };
    insert_row(conn, &row, era)
}

fn insert_legacy_session_point(
    conn: &Connection,
    row: &UsageEventRow,
    era: IngestEra,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                cost_usd
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NULL",
    )?;
    let existing: Vec<SessionStockPoint> = stmt
        .query_map(params![row.instance_id], |r| {
            Ok(SessionStockPoint {
                counters: SnapshotCounters {
                    total: r.get(0)?,
                    input: r.get(1)?,
                    output: r.get(2)?,
                    cache_read: r.get(3)?,
                    cache_write: r.get(4)?,
                },
                cost: parse_cost(r.get::<_, Option<String>>(5)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(row) = accept_session_point(row, &existing) else {
        return Ok(false);
    };
    insert_row(conn, &row, era)
}

fn insert_row(conn: &Connection, row: &UsageEventRow, era: IngestEra) -> rusqlite::Result<bool> {
    if row.scope == "message" && row.scope_id.is_some() {
        return upsert_message_row(conn, row, era);
    }
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
             metric_revision, source_session_id, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source, native_context_window, lineage_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            row.instance_id,
            row.seq,
            row.profile_id,
            row.model,
            row.scope,
            row.scope_id,
            row.turn_id,
            row.mode,
            row.metric_revision,
            row.source_session_id,
            row.total_tokens,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.cost_usd,
            row.accounting,
            row.observed_at,
            row.observed_at_source,
            row.native_context_window,
            era.lineage_value(),
        ],
    )?;
    Ok(inserted > 0)
}

fn upsert_message_row(
    conn: &Connection,
    row: &UsageEventRow,
    era: IngestEra,
) -> rusqlite::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source, native_context_window, lineage_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            row.instance_id,
            row.seq,
            row.profile_id,
            row.model,
            row.scope,
            row.scope_id,
            row.turn_id,
            row.mode,
            row.metric_revision,
            row.total_tokens,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.cost_usd,
            row.accounting,
            row.observed_at,
            row.observed_at_source,
            row.native_context_window,
            era.lineage_value(),
        ],
    )?;
    if inserted > 0 {
        return Ok(true);
    }
    // Duplicate durable key. Every accounting RHS compares the incoming
    // revision against the stored one (the RHS sees the pre-update row), so an
    // older revision is inert even while the revision column advances.
    let updated = conn.execute(
        "UPDATE usage_events SET
             metric_revision = CASE
                 WHEN ?12 > metric_revision THEN ?12
                 ELSE metric_revision END,
             turn_id = COALESCE(turn_id, ?3),
             total_tokens = CASE
                 WHEN ?4 IS NOT NULL AND ?12 >= metric_revision THEN ?4
                 ELSE total_tokens END,
             input_tokens = CASE
                 WHEN ?5 IS NOT NULL AND ?12 >= metric_revision THEN ?5
                 ELSE input_tokens END,
             output_tokens = CASE
                 WHEN ?6 IS NOT NULL AND ?12 >= metric_revision THEN ?6
                 ELSE output_tokens END,
             cache_read_tokens = CASE
                 WHEN ?7 IS NOT NULL AND ?12 >= metric_revision THEN ?7
                 ELSE cache_read_tokens END,
             cache_write_tokens = CASE
                 WHEN ?8 IS NOT NULL AND ?12 >= metric_revision THEN ?8
                 ELSE cache_write_tokens END,
             cost_usd = CASE
                 WHEN ?9 IS NOT NULL AND ?12 >= metric_revision THEN ?9
                 ELSE cost_usd END,
             accounting = CASE
                 WHEN ?12 >= metric_revision THEN ?13
                 ELSE accounting END,
             observed_at = CASE
                 WHEN observed_at_source = 'native' THEN observed_at
                 WHEN ?11 = 'native' THEN ?10
                 ELSE observed_at END,
             observed_at_source = CASE
                 WHEN observed_at_source = 'native' THEN 'native'
                 ELSE ?11 END,
             lineage_state = CASE
                 -- AE3: a strictly higher accepted revision carries the
                 -- incoming frame's era; an equal/lower (rejected) revision
                 -- and an equal-position metadata fill leave provenance stored.
                 WHEN ?12 > metric_revision THEN ?14
                 ELSE lineage_state END
         WHERE instance_id = ?1 AND scope = 'message' AND scope_id = ?2
           AND (
                metric_revision < ?12
                OR (turn_id IS NULL AND ?3 IS NOT NULL)
                OR (observed_at_source != 'native' AND ?11 = 'native')
                OR (?12 >= metric_revision AND (
                        (?4 IS NOT NULL AND total_tokens IS NOT ?4)
                     OR (?5 IS NOT NULL AND input_tokens IS NOT ?5)
                     OR (?6 IS NOT NULL AND output_tokens IS NOT ?6)
                     OR (?7 IS NOT NULL AND cache_read_tokens IS NOT ?7)
                     OR (?8 IS NOT NULL AND cache_write_tokens IS NOT ?8)
                     OR (?9 IS NOT NULL AND cost_usd IS NOT ?9)
                     OR (accounting IS NOT ?13)))
           )",
        params![
            row.instance_id,        // ?1
            row.scope_id,           // ?2
            row.turn_id,            // ?3
            row.total_tokens,       // ?4
            row.input_tokens,       // ?5
            row.output_tokens,      // ?6
            row.cache_read_tokens,  // ?7
            row.cache_write_tokens, // ?8
            row.cost_usd,           // ?9
            row.observed_at,        // ?10
            row.observed_at_source, // ?11
            row.metric_revision,    // ?12
            row.accounting,         // ?13
            era.lineage_value(),    // ?14
        ],
    )?;
    Ok(updated > 0)
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct SnapshotCounters {
    total: Option<i64>,
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
}

impl SnapshotCounters {
    fn of(row: &UsageEventRow) -> Self {
        Self {
            total: row.total_tokens,
            input: row.input_tokens,
            output: row.output_tokens,
            cache_read: row.cache_read_tokens,
            cache_write: row.cache_write_tokens,
        }
    }

    fn each(&self) -> [Option<i64>; 5] {
        [
            self.total,
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
        ]
    }

    fn token_baskets(&self) -> [Option<i64>; 4] {
        [self.input, self.output, self.cache_read, self.cache_write]
    }

    fn growth_against(&self, stored: &SnapshotCounters) -> (bool, bool) {
        let mut grew = false;
        let mut decreased = false;
        for (incoming, stored) in self.each().into_iter().zip(stored.each()) {
            if let Some(incoming) = incoming {
                match stored {
                    Some(stored) if incoming < stored => decreased = true,
                    Some(stored) if incoming > stored => grew = true,
                    None => grew = true,
                    _ => {}
                }
            }
        }
        (grew, decreased)
    }

    fn merged_over(self, retained: &SnapshotCounters) -> SnapshotCounters {
        let pick = |incoming: Option<i64>, kept: Option<i64>| incoming.or(kept);
        let input = pick(self.input, retained.input);
        let output = pick(self.output, retained.output);
        let cache_read = pick(self.cache_read, retained.cache_read);
        let cache_write = pick(self.cache_write, retained.cache_write);
        // Derive only when the incoming frame did not itself report a total:
        // the retained total is stale relative to a merged basket that moved.
        // U1: flatten would silently treat an unknown basket as zero, so fold
        // through a strict all-known sum instead.
        let baskets = [input, output, cache_read, cache_write];
        let derived = baskets
            .iter()
            .all(Option::is_some)
            .then(|| baskets.into_iter().flatten().reduce(i64::saturating_add))
            .flatten();
        let total = self.total.or(derived);
        SnapshotCounters {
            total,
            input,
            output,
            cache_read,
            cache_write,
        }
    }
}

fn turn_position_is_stale(incoming_position: i64, stored_revision: i64) -> bool {
    incoming_position <= stored_revision
}

fn upsert_turn_snapshot(
    conn: &Connection,
    row: &UsageEventRow,
    era: IngestEra,
) -> rusqlite::Result<bool> {
    // Look up ONLY the incoming lineage's own revision-replaced row. NULL is
    // matched with IS NULL; a qualified id with equality. Either way the
    // stored row's lineage IS the incoming lineage.
    let stored: Option<TurnFrame> = {
        let map_row = |r: &rusqlite::Row<'_>| {
            Ok(TurnFrame {
                rowid: Some(r.get(0)?),
                seq: r.get(1)?,
                profile_id: r.get(2)?,
                model: r.get(3)?,
                mode: r.get(4)?,
                revision: r.get(5)?,
                source_session_id: r.get(6)?,
                counters: SnapshotCounters {
                    total: r.get(7)?,
                    input: r.get(8)?,
                    output: r.get(9)?,
                    cache_read: r.get(10)?,
                    cache_write: r.get(11)?,
                },
                cost_usd: r.get(12)?,
                accounting: r.get(13)?,
                observed_at: r.get(14)?,
                observed_at_source: r.get(15)?,
                native_context_window: r.get(16)?,
                era: match r.get::<_, Option<String>>(17)? {
                    Some(v) if v == "legacy" => IngestEra::Legacy,
                    _ => IngestEra::Live,
                },
            })
        };
        const STORED_SELECT: &str = "SELECT rowid, seq, profile_id, model, mode,
                    metric_revision, source_session_id,
                    total_tokens, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens,
                    cost_usd, accounting, observed_at, observed_at_source,
                    native_context_window, lineage_state
             FROM usage_events
             WHERE instance_id = ?1 AND scope = 'turn' AND scope_id = ?2
               AND source_session_id {predicate}";
        match row.source_session_id.as_deref() {
            // Bind only the placeholders the SQL actually contains.
            Some(session) => conn
                .query_row(
                    &STORED_SELECT.replace("{predicate}", "= ?3"),
                    params![row.instance_id, row.scope_id, session],
                    map_row,
                )
                .ok(),
            None => conn
                .query_row(
                    &STORED_SELECT.replace("{predicate}", "IS NULL"),
                    params![row.instance_id, row.scope_id],
                    map_row,
                )
                .ok(),
        }
    };

    let Some(stored) = stored else {
        // First frame of this lineage for the scope id: its own row, leaving
        // every other lineage's row and watermark untouched. The row carries
        // THIS frame's producer era.
        return insert_row(conn, row, era);
    };

    // The authority gate runs BEFORE any field branch — including the
    // cost-only billing channel (c-ctxusage r15 item 3). An equal or older
    // position within THIS lineage touches nothing: no counters, no
    // equal-position growth or bucket fill, no bill correction, no clock. Two
    // frames at one position are one source record however often re-delivered
    // and whichever channels each delivery used; fills and corrections arrive
    // only with a strictly newer position.
    if turn_position_is_stale(row.metric_revision, stored.revision) {
        return Ok(false);
    }

    let incoming = TurnFrame {
        rowid: None,
        seq: row.seq,
        profile_id: row.profile_id.clone(),
        model: row.model.clone(),
        mode: row.mode.clone(),
        revision: row.metric_revision,
        source_session_id: row.source_session_id.clone(),
        counters: SnapshotCounters::of(row),
        cost_usd: row.cost_usd.clone(),
        accounting: row.accounting.clone(),
        observed_at: row.observed_at.clone(),
        observed_at_source: row.observed_at_source.clone(),
        native_context_window: row.native_context_window,
        era,
    };
    // One reconciliation for every frame shape: the cost-only channel carries
    // counters/clock forward and advances only position/lineage/bill; the
    // counter channel merges baskets and moves the clock under native trust.
    // The update is scoped to THIS lineage's row id and writes the lineage
    // explicitly (never COALESCE: an unqualified update targets the NULL row
    // and stays NULL, and can never inherit a retained qualified id).
    let reconciled = reconcile_turn_frame(&stored, &incoming);
    persist_reconciled_turn(conn, &reconciled)?;
    Ok(true)
}

fn knowledge_u64(value: &Value) -> Option<i64> {
    let inner = value.get("value")?;
    // U64 is serialized as a decimal string on the wire.
    if let Some(text) = inner.as_str() {
        return text.parse().ok();
    }
    inner.as_i64()
}

fn additive_knowledge_u64(value: Option<&Value>) -> Option<i64> {
    let payload = value?;
    if payload.get("state").and_then(Value::as_str) == Some("not-applicable") {
        return Some(0);
    }
    knowledge_u64(payload)
}

fn scalar_u64(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return text.parse().ok();
    }
    value.as_i64()
}

pub fn project_usage_event(
    record: &JournalRecord,
    profile_id: Option<&str>,
    model: Option<&str>,
) -> Option<UsageEventRow> {
    if record.event.get("kind").and_then(Value::as_str) != Some("usage") {
        return None;
    }
    let payload = record.event.get("payload")?;
    let total_tokens = payload.get("totalTokens").and_then(knowledge_u64);
    let input_tokens = additive_knowledge_u64(payload.get("inputTokens"));
    let output_tokens = additive_knowledge_u64(payload.get("outputTokens"));
    let cache_read_tokens = additive_knowledge_u64(payload.get("cacheReadTokens"));
    let cache_write_tokens = additive_knowledge_u64(payload.get("cacheWriteTokens"));
    // c-usagefu (c): native per-model context window, additively optional.
    let native_context_window = payload.get("contextWindow").and_then(scalar_u64);
    let cost_usd = payload
        .get("cost")
        .and_then(|cost| cost.get("value"))
        .and_then(|value| value.get("amount"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // c-ctxusage r2 item 4: rate windows (TPM / lastTurnAt) must reflect when
    // the model call actually happened, not when a re-hydrated transcript was
    // ingested. Prefer the observation's known native timestamp; fall back to
    // journal ingest time for frames that did not carry one. c-ctxusage r4
    // item 5 records which one it was, so replays can repair the fallback.
    let native_at = record
        .event
        .get("nativeAt")
        .and_then(|at| at.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let (observed_at, observed_at_source) = match native_at {
        Some(at) => (at, "native".to_string()),
        None => (record.observed_at.clone(), "ingest".to_string()),
    };
    Some(UsageEventRow {
        instance_id: record.instance_id.clone(),
        seq: record.seq,
        profile_id: profile_id.map(str::to_string),
        model: model.map(str::to_string),
        scope: payload
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("turn")
            .to_string(),
        scope_id: payload
            .get("scopeId")
            .and_then(Value::as_str)
            .map(str::to_string),
        // r6 item 5: the stamped envelope's native turn id lives under
        // `source.nativeTurnId` (camelCased ObservationSource).
        turn_id: record
            .event
            .get("source")
            .and_then(|source| source.get("nativeTurnId"))
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
            .map(str::to_string),
        // c-ctxusage r3 item 2: `metricRevision` is a bare protocol `U64`,
        // serialized as a DECIMAL STRING (`"2"`), not a `Knowledge<U64>` object
        // — reading it through `knowledge_u64` (which dives into `.value`)
        // yielded None and silently pinned every snapshot at revision 1.
        metric_revision: scalar_revision(payload.get("metricRevision")).unwrap_or(1),
        // c-ctxusage r15 item 4: session-qualified turn positions.
        // Ruling AE: the SAME identity resolver live ingest and journal replay
        // use. Prefer the payload's explicit `sourceSessionId`; older producers
        // (and envelope-only legacy observations) put the CONFIRMED session
        // binding only on the envelope `source.nativeSessionId`. Accept that
        // envelope fallback only when `source.nativeTurnId` is absent or equals
        // the payload turn scope id — a stamp for a DIFFERENT turn proves
        // nothing about this frame.
        source_session_id: payload
            .get("sourceSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let scope_turn = payload.get("scopeId").and_then(Value::as_str);
                let is_turn = payload.get("scope").and_then(Value::as_str) == Some("turn");
                if !is_turn {
                    return None;
                }
                let env_turn = record
                    .event
                    .get("source")
                    .and_then(|s| s.get("nativeTurnId"))
                    .and_then(|v| v.get("value"))
                    .and_then(Value::as_str);
                let turn_matches = match env_turn {
                    Some(t) => Some(t) == scope_turn,
                    None => true,
                };
                if !turn_matches {
                    return None;
                }
                record
                    .event
                    .get("source")
                    .and_then(|s| s.get("nativeSessionId"))
                    .and_then(|v| v.get("value"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
            }),
        mode: payload
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("snapshot")
            .to_string(),
        total_tokens,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost_usd,
        accounting: payload
            .get("accounting")
            .and_then(Value::as_str)
            .unwrap_or("estimated")
            .to_string(),
        observed_at,
        observed_at_source,
        native_context_window,
    })
}

fn scalar_revision(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return text.parse().ok();
    }
    value.as_i64()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAggregate {
    pub events: i64,
    pub ambiguous_events: i64,
    pub ambiguous: bool,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub unavailable: bool,
    pub token_event_rows: i64,
}

impl UsageAggregate {
    pub(crate) fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn budget_status(&self, max_usd: Option<f64>) -> BudgetStatus {
        if self.unavailable {
            return BudgetStatus::Ok;
        }
        let Some(cost) = self.cost_usd else {
            return BudgetStatus::Unknown;
        };
        if self.ambiguous {
            return BudgetStatus::Unknown;
        }
        let Some(max) = max_usd.filter(|v| *v > 0.0) else {
            return BudgetStatus::Ok;
        };
        if cost >= max * BUDGET_STOP_FACTOR {
            BudgetStatus::Stop
        } else if cost >= max {
            BudgetStatus::Warn
        } else {
            BudgetStatus::Ok
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetStatus {
    Ok,
    Warn,
    Stop,
    Unknown,
}

fn eligible_flow_predicate(outer: &str) -> String {
    let cover_linked =
        turn_fully_covered_sql(&format!("{outer}.instance_id"), &format!("{outer}.turn_id"));
    let cover_linked_turn = turn_fully_covered_sql(
        &format!("{outer}.instance_id"),
        &format!("{outer}.scope_id"),
    );
    format!(
        "(({outer}.scope = 'message' AND {outer}.turn_id IS NOT NULL
            AND (
                NOT EXISTS (
                    SELECT 1 FROM usage_events _ef_t
                    WHERE _ef_t.instance_id = {outer}.instance_id
                      AND _ef_t.scope = 'turn'
                      AND _ef_t.scope_id = {outer}.turn_id
                 )
                OR ({cover_linked})
            )
         )
         OR ({outer}.scope = 'message' AND {outer}.turn_id IS NULL
             AND NOT EXISTS (
                 SELECT 1 FROM usage_events _ef_lt
                 WHERE _ef_lt.instance_id = {outer}.instance_id
                   AND _ef_lt.scope = 'turn'
             )
         )
         OR ({outer}.scope = 'turn' AND NOT ({cover_linked_turn})))"
    )
}

fn turn_has_durable_message_sql(inst: &str, tid: &str, alias: &str) -> String {
    let marker = remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX;
    format!(
        "EXISTS (
            SELECT 1 FROM usage_events {alias}
            WHERE {alias}.instance_id = {inst}
              AND {alias}.scope = 'message'
              AND {alias}.turn_id = {tid}
              AND substr({alias}.scope_id, 1, {len}) = '{marker}'
        )",
        len = marker.len(),
    )
}

fn turn_fully_covered_sql(inst: &str, tid: &str) -> String {
    let buckets = [
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ];
    // At least one durable linked Message.
    let mut conds = vec![turn_has_durable_message_sql(inst, tid, "_fc_d")];
    for bucket in buckets {
        // Each condition is a full OR and MUST be parenthesized: the list is
        // joined with AND, and a bare `A IS NULL OR B = C` would let AND bind
        // tighter and silently flatten the coverage proof across buckets. The
        // sum is scoped to DURABLE linked Messages (the marker prefix). Both
        // Turn lookups select the FOLD WINNER (seq DESC, rowid DESC): a bare
        // scalar subquery over the per-lineage rows could return the losing
        // lineage (r37 item 5).
        let marker = remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX;
        conds.push(format!(
            "((SELECT _fc_t.{bucket} FROM usage_events _fc_t
              WHERE _fc_t.instance_id = {inst}
                AND _fc_t.scope = 'turn' AND _fc_t.scope_id = {tid}
              ORDER BY _fc_t.seq DESC, _fc_t.rowid DESC LIMIT 1) IS NULL
             OR COALESCE((
                SELECT SUM(_fc_s.{bucket}) FROM usage_events _fc_s
                WHERE _fc_s.instance_id = {inst}
                  AND _fc_s.scope = 'message' AND _fc_s.turn_id = {tid}
                  AND substr(_fc_s.scope_id, 1, {len}) = '{marker}'
             ), 0)
             = (SELECT _fc_t2.{bucket} FROM usage_events _fc_t2
                WHERE _fc_t2.instance_id = {inst}
                  AND _fc_t2.scope = 'turn' AND _fc_t2.scope_id = {tid}
                ORDER BY _fc_t2.seq DESC, _fc_t2.rowid DESC LIMIT 1))",
            len = marker.len(),
        ));
    }
    conds.join(" AND ")
}

fn fold_ctes_instance() -> &'static str {
    static C: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    C.get_or_init(|| unified_fold_ctes("instance_id = ?1", "global_reported_session_cost"))
}

fn fold_ctes_supply() -> &'static str {
    static C: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        // Supply folds the newest bill per MODEL+session (r39 item 4) AND the
        // applicable legacy NULL-scope bill (r40 item 5 / AB2).
        unified_fold_ctes(
            "profile_id = ?1 AND model = ?2",
            "global_reported_cost_supply",
        )
    })
}

fn unified_fold_ctes(domain_pred: &str, reported_source: &str) -> String {
    // Qualify a standalone-domain predicate ("instance_id = ?1" or
    // "profile_id = ?1 AND model = ?2") for a specific table alias. Word
    // boundaries are essential: a plain `replace("model", ..)` would also
    // rewrite the `id`/`instance_id` column text.
    let d = |alias: &str| {
        // Qualify each whole-identifier occurrence of the three domain
        // columns (they share substrings, so byte-level boundaries matter).
        let qualify = |input: &str, col: &str| -> String {
            let mut out = String::with_capacity(input.len());
            let bytes = input.as_bytes();
            let mut cursor = 0usize;
            while let Some(rel) = input[cursor..].find(col) {
                let idx = cursor + rel;
                let before_ok =
                    idx == 0 || !(bytes[idx - 1].is_ascii_alphanumeric() || bytes[idx - 1] == b'_');
                let after = idx + col.len();
                let after_ok = after == bytes.len()
                    || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
                out.push_str(&input[cursor..idx]);
                if before_ok && after_ok {
                    out.push_str(alias);
                    out.push('.');
                }
                out.push_str(col);
                cursor = after;
            }
            out.push_str(&input[cursor..]);
            out
        };
        let mut out = domain_pred.to_owned();
        for col in ["instance_id", "profile_id", "model"] {
            out = qualify(&out, col);
        }
        out
    };
    let message_eligible = eligible_flow_predicate("m");
    let turn_covered = turn_fully_covered_sql("t.instance_id", "t.scope_id");
    format!(
        "global_scoped_sessions AS (
            -- COMPLETE scoped-session relation (newest point per
            -- instance/scope_id): used for existence checks (legacy/scoped
            -- selection, ambiguity) and as the backbone for cost authority.
            -- It deliberately includes tokenless reported carriers — cost
            -- presence must not depend on tokens (AB2+).
            SELECT * FROM (
                SELECT rowid AS ev_rowid, *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id, scope_id ORDER BY seq DESC, rowid DESC
                ) AS stock_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NOT NULL
            )
            WHERE stock_rn = 1
         ),
         global_token_stock_sessions AS (
            -- r43 AC2 (ONE stock selection): the TOKEN relation ranks ONLY
            -- token-bearing points, so a later tokenless reported (or
            -- window-only) carrier can never displace an earlier stock's
            -- tokens by winning the ranking and then being filtered out.
            -- Cost authority/presence keep using the complete relation above.
            SELECT * FROM (
                SELECT rowid AS ev_rowid, *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id, scope_id ORDER BY seq DESC, rowid DESC
                ) AS token_stock_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NOT NULL
                  AND (total_tokens IS NOT NULL OR input_tokens IS NOT NULL
                       OR output_tokens IS NOT NULL OR cache_read_tokens IS NOT NULL
                       OR cache_write_tokens IS NOT NULL)
            )
            WHERE token_stock_rn = 1
         ),
         global_reported_session_cost AS (
            -- r38 item 3 / r39 items 2-4 (AA2): a result-frame Session carrier
            -- reports the native CUMULATIVE bill for one native session. The
            -- bill is the newest KNOWN reported value per (instance, session),
            -- folded once — never a sum (0.01 then 0.03 is 0.03). Newest KNOWN
            -- rather than newest row: a later window-only result carries no
            -- cost and must not erase the established bill.
            --
            -- r39 item 3: the legacy NULL-scope reported bill is suppressed
            -- once the instance has ANY scoped session evidence (the same
            -- instance-global legacy-vs-scoped selection as legacy_stock) —
            -- a $0.40 NULL bill followed by a scoped $0.50 bills $0.50, never
            -- $0.90. The two branches therefore partition distinct keys:
            -- scoped (instance, scope_id), and one NULL bill per legacy
            -- instance with no scoped sessions.
            SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                   total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens FROM (
                SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                       total_tokens, input_tokens, output_tokens,
                       cache_read_tokens, cache_write_tokens, ROW_NUMBER() OVER (
                    PARTITION BY instance_id, scope_id
                    ORDER BY CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END,
                             seq DESC, rowid DESC
                ) AS cost_rn
                FROM usage_events
                WHERE scope = 'session' AND accounting = 'reported'
                  AND scope_id IS NOT NULL
            )
            WHERE cost_rn = 1 AND cost_usd IS NOT NULL
            UNION ALL
            SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                   total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens FROM (
                SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                       total_tokens, input_tokens, output_tokens,
                       cache_read_tokens, cache_write_tokens,
                       ROW_NUMBER() OVER (
                           PARTITION BY instance_id
                           ORDER BY CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END,
                                    seq DESC, rowid DESC
                       ) AS legacy_cost_rn
                FROM usage_events u
                WHERE scope = 'session' AND accounting = 'reported'
                  AND scope_id IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM usage_events s
                      WHERE s.instance_id = u.instance_id
                        AND s.scope = 'session' AND s.scope_id IS NOT NULL
                  )
            )
            WHERE legacy_cost_rn = 1 AND cost_usd IS NOT NULL
         ),
         global_reported_cost_by_model AS (
            -- Newest KNOWN reported frame per (instance, profile, model,
            -- session). Used for PRESENCE only after r43 AC4: a model keeps a
            -- provider row after a /model switch, but superseded cumulative
            -- bills are never re-summed (the cross-model newest bill below is
            -- the single amount).
            SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                   total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens FROM (
                SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                       total_tokens, input_tokens, output_tokens,
                       cache_read_tokens, cache_write_tokens, ROW_NUMBER() OVER (
                    PARTITION BY instance_id, profile_id, model, scope_id
                    ORDER BY CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END,
                             seq DESC, rowid DESC
                ) AS model_cost_rn
                FROM usage_events
                WHERE scope = 'session' AND accounting = 'reported'
                  AND scope_id IS NOT NULL
            )
            WHERE model_cost_rn = 1 AND cost_usd IS NOT NULL
         ),
         global_reported_presence_supply AS (
            -- r43 AC4: superseded per-model reported frames keep the model
            -- VISIBLE (AA3 presence) but contribute neither cost nor tokens —
            -- the cumulative bill is counted once, on the newest cross-model
            -- bill's model.
            SELECT m.* FROM global_reported_cost_by_model m
            WHERE NOT EXISTS (
                SELECT 1 FROM global_reported_session_cost c
                WHERE c.rowid = m.rowid
            )
         ),
         global_reported_cost_supply AS (
            -- r43 AC4: supply AMOUNTS come from the SAME cross-model newest-
            -- bill relation the instance fold uses (including its legacy
            -- branch), attributed to the newest bill's model. Summing the
            -- per-model supplies therefore equals the instance aggregate; a
            -- newer cumulative bill on B no longer double-bills A.
            SELECT rowid, instance_id, seq, profile_id, model, scope_id, cost_usd,
                   total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens
            FROM global_reported_session_cost
         ),
         global_reported_cost_sessions AS (
            -- r41 AB2+: reported-authority keys computed GLOBALLY per
            -- (instance_id, native session) AFTER the instance-global
            -- legacy/scoped selection and BEFORE any provider/model
            -- projection. They key off the INSTANCE-fold source (newest bill
            -- per session across models): a bill reported under model B for
            -- session S therefore excludes S's estimate in the supply(A) fold
            -- as well — the covered estimate never rebills under another
            -- model. Model-ATTRIBUTED amounts are still summed from the
            -- domain-projected reported_session_cost below (AA3).
            SELECT DISTINCT instance_id, COALESCE(scope_id, '') AS sid
            FROM global_reported_session_cost
         ),
         global_reported_cost_instances AS (
            SELECT DISTINCT instance_id FROM global_reported_session_cost
         ),
         global_legacy_stock AS (
            -- COMPLETE newest legacy NULL-scope session point per instance
            -- (existence / legacy-scoped cost selection).
            SELECT * FROM (
                SELECT rowid AS ev_rowid, *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id ORDER BY rowid DESC
                ) AS legacy_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NULL
            )
            WHERE legacy_rn = 1
         ),
         global_token_legacy_stock AS (
            -- r43 AC2: the TOKEN legacy relation ranks only token-bearing
            -- legacy points (same pre-ranking exclusion as scoped token
            -- stock).
            SELECT * FROM (
                SELECT rowid AS ev_rowid, *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id ORDER BY rowid DESC
                ) AS token_legacy_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NULL
                  AND (total_tokens IS NOT NULL OR input_tokens IS NOT NULL
                       OR output_tokens IS NOT NULL OR cache_read_tokens IS NOT NULL
                       OR cache_write_tokens IS NOT NULL)
            )
            WHERE token_legacy_rn = 1
         ),
         global_estimated_session_points AS (
            -- r43 AC2: the COMPLETE cost relation for ESTIMATED cumulative
            -- stocks — newest ESTIMATED point per (instance, session)
            -- INCLUDING tokenless/window-only points (the stock-only
            -- Ruling-B bill). Independent of token ranking: a later tokenless
            -- reported carrier never removes an estimate, and a tokenless
            -- successor never masks an earlier Unknown estimate.
            SELECT * FROM (
                SELECT rowid AS ev_rowid, *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id, scope_id ORDER BY seq DESC, rowid DESC
                ) AS est_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NOT NULL
                  AND accounting <> 'reported'
            )
            WHERE est_rn = 1
         ),
         global_estimated_legacy_points AS (
            -- Same complete estimated relation for legacy NULL-scope points.
            SELECT * FROM (
                SELECT rowid AS ev_rowid, *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id ORDER BY rowid DESC
                ) AS est_legacy_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NULL
                  AND accounting <> 'reported'
            )
            WHERE est_legacy_rn = 1
         ),
         global_winning_turns AS (
            -- rowid is projected explicitly: the rate-window query correlates
            -- winning rows against g_ambiguous_turns BY rowid (NULL scope_ids
            -- cannot be joined by value), and a UNION ALL result exposes
            -- rowid only when both branches SELECT it under the same name.
            SELECT * FROM (
                SELECT rowid AS winning_rowid, t.*, ROW_NUMBER() OVER (
                    PARTITION BY t.instance_id, t.scope_id ORDER BY t.seq DESC, t.rowid DESC
                ) AS turn_rn
                FROM usage_events t
                WHERE t.scope = 'turn' AND t.scope_id IS NOT NULL
            )
            WHERE turn_rn = 1
            UNION ALL
            SELECT rowid AS winning_rowid, u.*, NULL AS turn_rn
            FROM usage_events u
            WHERE u.scope = 'turn' AND u.scope_id IS NULL
         ),
g_flow_messages AS (
            -- `eligible_flow_predicate` covers BOTH scopes (it is main's
            -- outer-relation predicate); restrict it to Messages here because
            -- winning Turns go through the lineage CTEs below. Computed
            -- GLOBALLY (instance-wide classification) before domain filtering.
            SELECT m.* FROM usage_events m
            WHERE m.scope = 'message' AND ({message_eligible})
         ),
         g_session_turn_flow AS (
            -- Per-native-session sums of the winning TURN rows (all of them:
            -- a stock can only be certified against the complete retained
            -- turn flow, including rows the durable Messages represent).
            SELECT t.instance_id AS instance_id,
                   t.source_session_id AS sid,
                   COUNT(*) AS turn_count,
                   MAX(t.metric_revision) AS max_revision,
                   SUM(t.total_tokens) AS sum_total,
                   SUM(t.input_tokens) AS sum_input,
                   SUM(t.output_tokens) AS sum_output,
                   SUM(t.cache_read_tokens) AS sum_cache_read,
                   SUM(t.cache_write_tokens) AS sum_cache_write,
                   COUNT(t.total_tokens) AS known_total,
                   COUNT(t.input_tokens) AS known_input,
                   COUNT(t.output_tokens) AS known_output,
                   COUNT(t.cache_read_tokens) AS known_cache_read,
                   COUNT(t.cache_write_tokens) AS known_cache_write
            FROM global_winning_turns t
            WHERE t.source_session_id IS NOT NULL
            GROUP BY t.instance_id, t.source_session_id
         ),
         g_fully_covered_sessions AS (
            -- The latest cumulative stock certifies its turn flow: a strictly
            -- NEWER position is authoritative whatever the retained sums; at
            -- an equal position every bucket the stock reports must match the
            -- turn sums exactly. A stock ABOVE the retained flow (partial
            -- coverage) cannot certify: Ruling B flow wins and stock stands
            -- down (r9 items 4/5).
            --
            -- r43 AC2 (ONE stock selection): coverage certification reads the
            -- token-only ranking, so ANY tokenless point (reported cost-only
            -- carrier or window-only estimated point) is excluded before
            -- certification rather than filtered out afterward. A tokenless
            -- point can never hide its session's turn flow.
            SELECT s.instance_id AS instance_id, s.scope_id AS sid
            FROM global_token_stock_sessions s
            JOIN g_session_turn_flow f
              ON f.instance_id = s.instance_id AND f.sid = s.scope_id
            WHERE f.turn_count > 0
              AND (
                  f.max_revision < s.metric_revision
                  OR (
                      f.max_revision = s.metric_revision
                      AND (s.total_tokens IS NULL
                           OR (f.known_total = f.turn_count
                               AND f.sum_total = s.total_tokens))
                      AND (s.input_tokens IS NULL
                           OR (f.known_input = f.turn_count
                               AND f.sum_input = s.input_tokens))
                      AND (s.output_tokens IS NULL
                           OR (f.known_output = f.turn_count
                               AND f.sum_output = s.output_tokens))
                      AND (s.cache_read_tokens IS NULL
                           OR (f.known_cache_read = f.turn_count
                               AND f.sum_cache_read = s.cache_read_tokens))
                      AND (s.cache_write_tokens IS NULL
                           OR (f.known_cache_write = f.turn_count
                               AND f.sum_cache_write = s.cache_write_tokens))
                  )
              )
         ),
         g_proven_flow_turns AS (
            -- A lineage-qualified winning turn counts when its session's stock
            -- does NOT certify the turn flow and durable Messages do not cover
            -- it (durable coverage suppresses the Turn in favour of Messages).
            SELECT t.* FROM global_winning_turns t
            WHERE t.source_session_id IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM g_fully_covered_sessions c
                  WHERE c.instance_id = t.instance_id AND c.sid = t.source_session_id
              )
              AND NOT ({turn_covered})
         ),
         g_unmatched_flow_turns AS (
            -- Live per-call flow for an identified turn with no proven session
            -- lineage (Ruling B): bills alongside Messages and makes stock
            -- stand down. Pre-upgrade 'legacy' rows take the conservative
            -- ambiguity/open paths instead.
            SELECT t.* FROM global_winning_turns t
            WHERE t.scope_id IS NOT NULL
              AND t.source_session_id IS NULL
              AND COALESCE(t.lineage_state, '') <> 'legacy'
              AND NOT ({turn_covered})
         ),
         g_legacy_open_turns AS (
            -- A legacy (or NULL-identity) turn when NO scoped stock exists:
            -- ordinary per-call flow. Disjoint from g_proven_flow_turns: a
            -- migration-tagged row that is lineage-QUALIFIED retained its
            -- source_session_id through the upgrade and bills through the
            -- proven branch alone (r37 item 2).
            SELECT t.* FROM global_winning_turns t
            WHERE t.source_session_id IS NULL
              AND (
                  COALESCE(t.lineage_state, '') = 'legacy'
                  OR t.scope_id IS NULL
              )
              AND NOT EXISTS (
                  SELECT 1 FROM global_scoped_sessions s
                  WHERE s.instance_id = t.instance_id
              )
              AND NOT ({turn_covered})
         ),
         g_legacy_flow_turns AS (
            -- An identified pre-upgrade turn beside scoped stock bills when
            -- another per-call exists (Ruling B flow mode instance-wide).
            -- source_session_id IS NULL keeps this disjoint from the proven
            -- branch: a qualified retained lineage follows per-session
            -- certification, never this instance-wide legacy path (r37 item 2).
            SELECT t.* FROM global_winning_turns t
            WHERE t.scope_id IS NOT NULL
              AND t.source_session_id IS NULL
              AND COALESCE(t.lineage_state, '') = 'legacy'
              AND EXISTS (
                  SELECT 1 FROM global_scoped_sessions s
                  WHERE s.instance_id = t.instance_id
              )
              AND EXISTS (
                  SELECT 1 FROM usage_events e
                  WHERE e.instance_id = t.instance_id
                    AND (
                        e.scope = 'message'
                        OR (e.scope = 'turn'
                            AND e.scope_id IS NOT NULL
                            AND e.scope_id <> t.scope_id)
                    )
              )
              AND NOT ({turn_covered})
         ),
         g_ambiguous_turns AS (
            -- Unprovable turn beside scoped stock: dropped from sums, poisoned
            -- to Unknown (c-ctxusage r29 ambiguity / r34 NULL-identity supply
            -- flag). A live post-upgrade identified turn is never ambiguous.
            SELECT t.* FROM global_winning_turns t
            WHERE t.source_session_id IS NULL
              AND EXISTS (
                  SELECT 1 FROM global_scoped_sessions s
                  WHERE s.instance_id = t.instance_id
              )
              AND NOT ({turn_covered})
              AND (
                  t.scope_id IS NULL
                  OR (
                      COALESCE(t.lineage_state, '') = 'legacy'
                      AND NOT EXISTS (
                          SELECT 1 FROM usage_events e
                          WHERE e.instance_id = t.instance_id
                            AND (
                                e.scope = 'message'
                                OR (e.scope = 'turn'
                                    AND e.scope_id IS NOT NULL
                                    AND e.scope_id <> t.scope_id)
                            )
                      )
                  )
              )
         ),
         g_flow_turns AS (
            SELECT * FROM g_proven_flow_turns
            UNION ALL SELECT * FROM g_unmatched_flow_turns
            UNION ALL SELECT * FROM g_legacy_open_turns
            UNION ALL SELECT * FROM g_legacy_flow_turns
         ),
         override_instances AS (
            -- Instance-wide Ruling B stock-stand-down: triggered by durable
            -- Messages or by a surviving UNQUALIFIED live/legacy turn flow.
            -- Lineage-PROVEN turns are deliberately NOT here: they follow the
            -- per-session coverage/certification rule instead (a covered stock
            -- still counts when another session has an uncovered proven turn).
            --
            -- r40 AB2 / r41 AB2+: this is TOKEN-flow evidence only. Cost
            -- accounting never removes an instance — the COST fold keys
            -- authority per (instance, native session) in
            -- global_reported_cost_sessions below (global before projection).
            SELECT instance_id FROM g_flow_messages
            UNION
            SELECT instance_id FROM g_unmatched_flow_turns
            UNION
            SELECT instance_id FROM g_legacy_open_turns
            UNION
            SELECT instance_id FROM g_legacy_flow_turns
         ),
         -- DOMAIN PROJECTION: the classification above is instance-global so a
         -- row stays correctly suppressed/ambiguous under every supply view;
         -- only the final projection filters by instance or profile/model.
         --
         -- TOKEN FOLD (AB2): the four relations below are the ONLY token
         -- relations every consumer (instance rollup, supply aggregate,
         -- budget) reads. Membership is the coverage/certification/Ruling-B
         -- rule and NOTHING else — cost accounting never filters, adds or
         -- removes a token row.
         flow_messages AS (
            SELECT m.* FROM g_flow_messages m WHERE {dm}
         ),
         flow_turns AS (
            SELECT t.* FROM g_flow_turns t WHERE {dt}
         ),
         ambiguous_turns AS (
            SELECT t.* FROM g_ambiguous_turns t WHERE {dt}
         ),
         stock_sessions AS (
            -- TOKEN stock (r43 AC2): the token-only ranked relation, so
            -- membership is accounting-agnostic and tokenless points of ANY
            -- kind never enter — they cannot displace earlier token stock.
            -- Cost authority/presence use the complete Session relations
            -- instead. Flow-less sessions (no winning turn flow on the
            -- session) keep their stock unless a Ruling B instance-wide
            -- override stands it down.
            SELECT g.*, g.scope_id AS sid FROM global_token_stock_sessions g
            WHERE {dg}
              AND g.instance_id NOT IN (SELECT instance_id FROM override_instances)
              AND NOT EXISTS (
                  SELECT 1 FROM g_session_turn_flow f
                  WHERE f.instance_id = g.instance_id AND f.sid = g.scope_id
              )
            UNION ALL
            -- Sessions with lineage-certified turn flow: the stock counts
            -- when it certifies the flow (equal/newer position), provided no
            -- instance-wide Ruling B override is active; a surviving uncovered
            -- proven turn of THIS session drops the stock via the missing
            -- coverage match.
            SELECT g.*, g.scope_id AS sid FROM global_token_stock_sessions g
            WHERE {dg}
              AND g.instance_id NOT IN (SELECT instance_id FROM override_instances)
              AND EXISTS (
                  SELECT 1 FROM g_fully_covered_sessions c
                  WHERE c.instance_id = g.instance_id AND c.sid = g.scope_id
              )
         ),
         legacy_stock AS (
            -- TOKEN legacy stock from the token-only legacy ranking; the
            -- legacy/scoped existence guard still uses the COMPLETE relation.
            SELECT l.*, '' AS sid FROM global_token_legacy_stock l
            WHERE {dl}
              AND l.instance_id NOT IN (
                  SELECT instance_id FROM global_scoped_sessions
              )
              AND l.instance_id NOT IN (
                  SELECT instance_id FROM override_instances
              )
              AND NOT EXISTS (
                  SELECT 1 FROM g_flow_turns p WHERE p.instance_id = l.instance_id
              )
         ),
         -- COST FOLD (AB2): authority keyed by (instance_id, native_session_id)
         -- AFTER the instance-global legacy-vs-scoped selection.
         reported_session_cost AS (
            -- Newest KNOWN reported cumulative bill per (instance, session)
            -- for the instance fold, or per (model, session) PLUS the
            -- applicable legacy NULL bill for the supply fold; the selected
            -- source already applied the instance-global suppression once
            -- scoped evidence exists. Domain-projected — these are the
            -- ATTRIBUTED AMOUNTS (and presence), not the authority keys
            -- (those are global_reported_cost_sessions, r41 AB2+).
            SELECT g.* FROM {reported_source} g WHERE {dg}
         ),
         estimated_stock_rows AS (
            -- Projected ESTIMATED stock points that actually stand as the
            -- session's stock estimate (no Ruling-B override; flow-less or
            -- coverage-certified, matching the token stock membership) and
            -- whose session has no reported authority key. Complete wrt
            -- tokens: tokenless estimated bills stay here (stock-only case).
            SELECT g.* FROM global_estimated_session_points g
            WHERE {dg}
              AND g.instance_id NOT IN (SELECT instance_id FROM override_instances)
              AND NOT EXISTS (
                  SELECT 1 FROM global_reported_cost_sessions k
                  WHERE k.instance_id = g.instance_id AND k.sid = g.scope_id
              )
              AND (
                  NOT EXISTS (
                      SELECT 1 FROM g_session_turn_flow f
                      WHERE f.instance_id = g.instance_id AND f.sid = g.scope_id
                  )
                  OR EXISTS (
                      SELECT 1 FROM g_fully_covered_sessions c
                      WHERE c.instance_id = g.instance_id AND c.sid = g.scope_id
                  )
              )
            UNION ALL
            SELECT l.* FROM global_estimated_legacy_points l
            WHERE {dl}
              AND l.instance_id NOT IN (SELECT instance_id FROM global_scoped_sessions)
              AND l.instance_id NOT IN (SELECT instance_id FROM override_instances)
              AND NOT EXISTS (
                  SELECT 1 FROM g_flow_turns p WHERE p.instance_id = l.instance_id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM global_reported_cost_sessions k
                  WHERE k.instance_id = l.instance_id AND k.sid = ''
              )
         ),
         estimated_stock_cost AS (
            -- Selected scoped/legacy stock ESTIMATES (NULL stays: the
            -- sticky-Unknown poison), independent of token membership.
            SELECT cost_usd AS cost_usd FROM estimated_stock_rows
         ),
         unknown_reported_stock AS (
            -- r44 item 1: a REPORTED token-bearing stock whose cumulative
            -- bill is Unknown keeps cost Unknown — it must NOT fall through
            -- to the ELSE $0. Only a session WITHOUT a newer KNOWN reported
            -- bill poisons; a known bill elsewhere in the same session is the
            -- authority and covers it. Based on the standing TOKEN stock
            -- relations so it matches exactly the token evidence that surfaces.
            SELECT 1 AS hit FROM stock_sessions g
            WHERE g.accounting = 'reported'
              AND g.cost_usd IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM global_reported_cost_sessions k
                  WHERE k.instance_id = g.instance_id AND k.sid = g.scope_id
              )
            UNION ALL
            SELECT 1 FROM legacy_stock l
            WHERE l.accounting = 'reported'
              AND l.cost_usd IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM global_reported_cost_sessions k
                  WHERE k.instance_id = l.instance_id AND k.sid = ''
              )
         ),
         estimated_turn_cost AS (
            -- Uncovered flow-turn estimates, excluded by the GLOBAL authority
            -- keys. A lineage-QUALIFIED turn is excluded only when its OWN
            -- (instance, session) has a reported bill in ANY model; a proven
            -- other session keeps billing. An UNQUALIFIED turn cannot be
            -- proven to be another session, so ANY reported bill of its
            -- instance stands it down.
            SELECT t.cost_usd AS cost_usd
            FROM flow_turns t
            WHERE (
                t.source_session_id IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1 FROM global_reported_cost_sessions k
                    WHERE k.instance_id = t.instance_id
                      AND k.sid = t.source_session_id
                )
            )
            OR (
                t.source_session_id IS NULL
                AND t.instance_id NOT IN (
                    SELECT instance_id FROM global_reported_cost_instances
                )
            )
         ),
         estimated_message_cost AS (
            -- Durable Messages have no session attribution: excluded only
            -- when the WHOLE instance carries a reported bill; an unpriced
            -- Message contributes 0 (Ruling B per-call billing), never poison.
            SELECT m.cost_usd AS cost_usd
            FROM flow_messages m
            WHERE m.instance_id NOT IN (
                SELECT instance_id FROM global_reported_cost_instances
            )
         ),
         token_events AS (
            -- The TOKEN fold's rows (cost_usd is carried because selected
            -- stocks/turns may be priced, but cost accounting never shapes
            -- membership).
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_usd
            FROM flow_turns
            UNION ALL
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_usd
            FROM flow_messages
            UNION ALL
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_usd
            FROM stock_sessions
            UNION ALL
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_usd
            FROM legacy_stock
         ),
         contributions AS (
            -- PRESENCE (AB2 item 6 / r43 AC2): a domain/aggregate exists if
            -- it has TOKEN evidence, an attributable reported bill, OR a
            -- selected estimated stock (a stock-only tokenless cumulative
            -- bill is real spend evidence). The non-token branches contribute
            -- NULL token columns — token values come ONLY from token_events,
            -- so an unselected snapshot never leaks in.
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens
            FROM token_events
            UNION ALL
            SELECT NULL, NULL, NULL, NULL, NULL
            FROM reported_session_cost rc
            WHERE NOT EXISTS (
                SELECT 1 FROM stock_sessions s WHERE s.ev_rowid = rc.rowid
            )
            AND NOT EXISTS (
                SELECT 1 FROM legacy_stock l WHERE l.ev_rowid = rc.rowid
            )
            UNION ALL
            -- Tokenless estimated stock points keep the domain alive.
            SELECT NULL, NULL, NULL, NULL, NULL
            FROM estimated_stock_rows e
            WHERE e.total_tokens IS NULL AND e.input_tokens IS NULL
              AND e.output_tokens IS NULL AND e.cache_read_tokens IS NULL
              AND e.cache_write_tokens IS NULL
            UNION ALL
            -- Superseded per-model reported frames keep the model VISIBLE
            -- (r43 AC4 presence) with zero amount/tokens.
            SELECT NULL, NULL, NULL, NULL, NULL
            FROM global_reported_presence_supply p
            WHERE {dp}
         )",
        dm = d("m"),
        dt = d("t"),
        dg = d("g"),
        dl = d("l"),
        dp = d("p"),
        reported_source = reported_source,
    )
}

fn token_bucket_expr(bucket: &str) -> String {
    format!(
        "(SELECT CASE
             WHEN EXISTS (
                 SELECT 1 FROM ambiguous_turns WHERE {bucket} IS NOT NULL
             ) THEN NULL
             WHEN COUNT(*) = 0
                  AND (SELECT COUNT(*) FROM flow_messages) > 0 THEN 0
             WHEN COUNT(*) = 0 THEN NULL
             WHEN COUNT(*) = COUNT({bucket}) THEN SUM({bucket})
         END
        FROM (
            SELECT {bucket} FROM flow_turns
            UNION ALL SELECT {bucket} FROM stock_sessions
            UNION ALL SELECT {bucket} FROM legacy_stock
        )
    )
    + COALESCE((SELECT SUM({bucket}) FROM flow_messages), 0)"
    )
}

fn aggregate_select_sql() -> &'static str {
    static C: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        // One U1 bucket column: ambiguous-then-unknown gate over the
        // turn/stock half (empty half falls back to the independent Message
        // sum: 0 with Messages, Unknown without), plus Message SUM.
        format!(
            "SELECT
        (SELECT COUNT(*) FROM contributions),
        (SELECT COUNT(*) FROM ambiguous_turns),
        {total},
        {input},
        {output},
        {cache_read},
        {cache_write},
        -- r40 AB2: cost is the independent COST fold. Presence comes from
        -- contributions (token evidence OR an attributable reported bill), so
        -- a retained bill with zero token contributions keeps this column
        -- non-NULL (AB2 item 6 / AA3). Bills are newest-KNOWN per
        -- (instance, native session); estimates belong only to sessions
        -- WITHOUT such a bill — a different session of the same instance is
        -- never suppressed, and its Unknown cost poisons the aggregate.
        CASE
            WHEN (SELECT COUNT(*) FROM contributions) = 0 THEN NULL
            -- An ambiguous (always unqualified) turn's known cost poisons
            -- only an instance WITHOUT any reported bill.
            WHEN EXISTS (
                SELECT 1 FROM ambiguous_turns a
                WHERE a.cost_usd IS NOT NULL
                  AND a.instance_id NOT IN (
                      SELECT instance_id FROM global_reported_cost_instances
                  )
            ) THEN NULL
            -- Sticky-Unknown stock/legacy estimate of an uncovered session
            -- poisons (a covered session's stock is excluded upstream).
            WHEN EXISTS (
                SELECT 1 FROM estimated_stock_cost WHERE cost_usd IS NULL
            ) THEN NULL
            -- r44 item 1: a REPORTED token stock with an Unknown cumulative
            -- cost (and no newer known bill in its session) poisons too.
            WHEN EXISTS (
                SELECT 1 FROM unknown_reported_stock
            ) THEN NULL
            -- Unknown is preserved over the selected estimated contribution
            -- of EVERY uncovered native session (r41 AB2+):
            --  * a lineage-QUALIFIED unpriced turn poisons when its OWN
            --    (instance, session) has no reported bill — an unrelated
            --    session's bill never silences it;
            --  * an UNQUALIFIED unpriced turn stays instance-conservative:
            --    it poisons only on a turn-only instance (no session rows)
            --    with no reported bill anywhere in the instance.
            WHEN EXISTS (
                SELECT 1 FROM flow_turns ft
                WHERE ft.cost_usd IS NULL
                  AND (
                      (ft.source_session_id IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM global_reported_cost_sessions k
                           WHERE k.instance_id = ft.instance_id
                             AND k.sid = ft.source_session_id
                       ))
                   OR (ft.source_session_id IS NULL
                       AND ft.instance_id NOT IN (
                           SELECT instance_id FROM global_reported_cost_instances
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM usage_events sx
                           WHERE sx.instance_id = ft.instance_id
                             AND sx.scope = 'session'
                       ))
                  )
            ) THEN NULL
            ELSE
                -- Reported cumulative bills, once per attributable session.
                COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM reported_session_cost
                ), 0.0)
                -- Estimates of sessions WITHOUT a reported bill.
                + COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM estimated_stock_cost
                ), 0.0)
                + COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM estimated_turn_cost
                ), 0.0)
                -- Durable Message flow: an unpriced Message contributes 0
                -- (Ruling B per-call billing, c-usagefu r10).
                + COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM estimated_message_cost
                ), 0.0)
        END,
        -- r43 AC4: token-evidence row count (presence-only bill => 0).
        {token_rows}
     -- No FROM: every column is a scalar subquery over the fold CTEs, so the
     -- row must materialize even when `contributions` is empty (an empty
     -- aggregate SELECT over a zero-row FROM would otherwise return no rows).
     ",
            total = token_bucket_expr("total_tokens"),
            input = token_bucket_expr("input_tokens"),
            output = token_bucket_expr("output_tokens"),
            cache_read = token_bucket_expr("cache_read_tokens"),
            cache_write = token_bucket_expr("cache_write_tokens"),
            token_rows = "(SELECT COUNT(*) FROM token_events)",
        )
    })
}

fn row_to_aggregate(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageAggregate> {
    let ambiguous: i64 = row.get(1)?;
    Ok(UsageAggregate {
        // An empty fold returns one all-NULL row from the scalar subqueries;
        // its proven-event count is zero in that case.
        events: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
        ambiguous_events: ambiguous,
        ambiguous: ambiguous != 0,
        total_tokens: row.get(2)?,
        input_tokens: row.get(3)?,
        output_tokens: row.get(4)?,
        cost_usd: row.get(7)?,
        unavailable: false,
        token_event_rows: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
    })
}

pub fn aggregate_instance(
    conn: &Connection,
    instance_id: &str,
) -> rusqlite::Result<UsageAggregate> {
    // c-usagefu r15 item 1: a rolled-back migration leaves a shape the
    // new-schema aggregate SQL cannot run against. c-usagefu r16 item 7: carry
    // an explicit Unavailable state (never a zero that the client reads as
    // real spend) while keeping the endpoint usable.
    if !usage_schema_is_current(conn)? {
        return Ok(UsageAggregate::unavailable());
    }
    let ctes = fold_ctes_instance();
    let sql = format!("WITH {ctes} {select}", select = aggregate_select_sql());
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_row(params![instance_id], row_to_aggregate)
}

pub fn aggregate_supply(
    conn: &Connection,
    profile_id: &str,
    model: &str,
) -> rusqlite::Result<UsageAggregate> {
    if !usage_schema_is_current(conn)? {
        return Ok(UsageAggregate::unavailable());
    }
    let ctes = fold_ctes_supply();
    // Count surviving rows in THIS domain first: a zero-row fold is "model
    // absent from the data", not "model spent zero", so its token/cost
    // columns must surface as NULL rather than the empty-fold zero defaults.
    let count_sql = format!("WITH {ctes} SELECT COUNT(*) FROM contributions");
    let total_rows: i64 = {
        let mut stmt = conn.prepare_cached(&count_sql)?;
        stmt.query_row(params![profile_id, model], |row| row.get(0))?
    };
    let sql = format!("WITH {ctes} {select}", select = aggregate_select_sql());
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_row(params![profile_id, model], |row| {
        Ok(UsageAggregate {
            events: if total_rows == 0 {
                0
            } else {
                row.get::<_, Option<i64>>(0)?.unwrap_or(0)
            },
            ambiguous_events: row.get(1)?,
            ambiguous: row.get::<_, i64>(1)? != 0,
            total_tokens: if total_rows == 0 { None } else { row.get(2)? },
            input_tokens: if total_rows == 0 { None } else { row.get(3)? },
            output_tokens: if total_rows == 0 { None } else { row.get(4)? },
            cost_usd: if total_rows == 0 { None } else { row.get(7)? },
            unavailable: false,
            token_event_rows: if total_rows == 0 {
                0
            } else {
                row.get::<_, Option<i64>>(8)?.unwrap_or(0)
            },
        })
    })
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceUsageRollup {
    pub unavailable: bool,
    pub context_used_tokens: Option<i64>,
    pub context_window_tokens: Option<i64>,
    pub context_pct: Option<i64>,
    pub context_pct_approximate: bool,
    pub session_input_tokens: Option<i64>,
    pub session_output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub turns: i64,
    pub tpm_in_60s: Option<i64>,
    pub tpm_out_60s: Option<i64>,
    pub tpm_in_5m: Option<i64>,
    pub tpm_out_5m: Option<i64>,
    pub last_turn_at: Option<String>,
}

fn threshold_rfc3339(seconds: i64) -> String {
    let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(seconds);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

fn sum_turn_tokens_since(
    conn: &Connection,
    instance_id: &str,
    since: &str,
) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    // Fold to the lineage WINNER per scope id BEFORE the native-time/window
    // filter (c-ctxusage r17 item 5 / r22 item 1): ranking inside the window
    // would count a superseded in-window lineage, and filtering native rows
    // first would let an ingest-timed newer winner lose to an older native
    // row. Winners then pass the unified flow relation (message coverage
    // proof, session provenance, ambiguity) from [`unified_fold_ctes`].
    let ctes = fold_ctes_instance();
    let turn_covered = turn_fully_covered_sql("t.instance_id", "t.scope_id");
    let window_sql = format!(
        "WITH {ctes},
             -- Rate windows are PER-CALL throughput: lineage-certified turns
             -- still count even when the current stock certifies them (the
             -- stock is cumulative, not throughput — c-ctxusage r4 item 6);
             -- only a turn fully re-emitted as durable MESSAGES is excluded.
             -- Turn-frame U1 and the independent Message sum are kept apart.
             win_turn AS (
                SELECT t.input_tokens, t.output_tokens
                FROM global_winning_turns t
                WHERE t.instance_id = ?1
                  AND t.observed_at_source = 'native'
                  AND t.observed_at >= ?2
                  AND NOT ({turn_covered})
                  AND NOT EXISTS (
                      -- Exclude exactly the rows the fold classifies as
                      -- ambiguous (identified unproven non-legacy turns beside
                      -- scoped stock). Legacy/NULL-identity turns and lineage-
                      -- certified turns are kept even if their scope id is
                      -- NULL at SQL correlation time.
                      SELECT 1 FROM g_ambiguous_turns a
                      WHERE a.winning_rowid = t.winning_rowid
                  )
             ),
             win_msg AS (
                SELECT input_tokens, output_tokens FROM flow_messages
                WHERE observed_at_source = 'native' AND observed_at >= ?2
             )
             SELECT
                -- No rows at all in the window => Unknown (not a zero rate).
                -- A turn/stock frame half is U1-gated: a NULL bucket poisons
                -- the whole window half; per-response Messages sum
                -- independently (NULL bucket = zero for that response).
                CASE
                    WHEN (SELECT COUNT(*) FROM win_turn) = 0
                         AND (SELECT COUNT(*) FROM win_msg) = 0 THEN NULL
                    WHEN (SELECT COUNT(*) FROM win_turn) = 0
                         THEN COALESCE((SELECT SUM(input_tokens) FROM win_msg), 0)
                    ELSE (SELECT CASE
                             WHEN COUNT(*) = COUNT(input_tokens) THEN SUM(input_tokens)
                          END FROM win_turn)
                         + COALESCE((SELECT SUM(input_tokens) FROM win_msg), 0)
                END,
                CASE
                    WHEN (SELECT COUNT(*) FROM win_turn) = 0
                         AND (SELECT COUNT(*) FROM win_msg) = 0 THEN NULL
                    WHEN (SELECT COUNT(*) FROM win_turn) = 0
                         THEN COALESCE((SELECT SUM(output_tokens) FROM win_msg), 0)
                    ELSE (SELECT CASE
                             WHEN COUNT(*) = COUNT(output_tokens) THEN SUM(output_tokens)
                          END FROM win_turn)
                         + COALESCE((SELECT SUM(output_tokens) FROM win_msg), 0)
                END"
    );
    let mut window_stmt = conn.prepare_cached(&window_sql)?;
    window_stmt.query_row(params![instance_id, since], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
}

#[derive(Clone, Default)]
struct StockIncrement {
    input: Option<i64>,
    output: Option<i64>,
    end_at: String,
}

fn session_stock_increments(
    conn: &Connection,
    instance_id: &str,
) -> rusqlite::Result<Vec<StockIncrement>> {
    // Per scope id (c-ctxusage r5 item 3A): s1→s2 is a session boundary, not a
    // reset-within-one-stock. Diff each session's ordered points independently,
    // then collect the increments. Legacy NULL-scope points chain on their own.
    let scoped = session_point_rows(conn, instance_id, true)?;
    if scoped.is_empty() {
        let legacy = session_point_rows(conn, instance_id, false)?;
        return diff_rows(legacy);
    }
    diff_rows(scoped)
}

type SessionPointRow = (Option<i64>, Option<i64>, String, Option<String>, String);

fn session_point_rows(
    conn: &Connection,
    instance_id: &str,
    scoped: bool,
) -> rusqlite::Result<Vec<SessionPointRow>> {
    let predicate = if scoped {
        "scope_id IS NOT NULL"
    } else {
        "scope_id IS NULL"
    };
    let sql = format!(
        "SELECT input_tokens, output_tokens, observed_at, scope_id,
                observed_at_source
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session' AND {predicate}
         ORDER BY scope_id ASC, rowid ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(params![instance_id], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })?
    .collect()
}

fn diff_rows(rows: Vec<SessionPointRow>) -> rusqlite::Result<Vec<StockIncrement>> {
    let mut increments = Vec::new();
    let mut previous: Option<(Option<String>, Option<i64>, Option<i64>)> = None;
    for (input, output, end_at, scope_id, source) in rows {
        let boundary = previous
            .as_ref()
            .map(|(prev_scope, _, _)| *prev_scope != scope_id)
            .unwrap_or(true);
        let trusted_time = source == "native" || source == "legacy";
        if !boundary
            && trusted_time
            && let Some((_, prev_input, prev_output)) = &previous
        {
            // None for either side of a bucket means "unknown for this
            // interval": only two KNOWN adjacent counters produce a number.
            let bucket = |current: Option<i64>, previous: Option<i64>| -> Option<i64> {
                match (current, previous) {
                    (Some(current), Some(previous)) => Some(if current < previous {
                        // Saturating reset: the stock restarted at `current`.
                        current
                    } else {
                        current - previous
                    }),
                    _ => None,
                }
            };
            increments.push(StockIncrement {
                input: bucket(input, *prev_input),
                output: bucket(output, *prev_output),
                end_at,
            });
        }
        // Every accepted point — ingest or native — is the next point's
        // immediate baseline.
        previous = Some((scope_id, input, output));
    }
    Ok(increments)
}

fn sum_session_increments_since(
    conn: &Connection,
    instance_id: &str,
    since: &str,
) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    let increments = session_stock_increments(conn, instance_id)?;
    let in_window: Vec<_> = increments
        .into_iter()
        .filter(|increment| increment.end_at.as_str() >= since)
        .collect();
    if in_window.is_empty() {
        // No trustworthy increment inside the window — unknown, never stock.
        return Ok((None, None));
    }
    let mut input: Option<i64> = Some(0);
    let mut output: Option<i64> = Some(0);
    let mut input_unknown = false;
    let mut output_unknown = false;
    for increment in in_window {
        // U1 (c-ctxusage r17 item 6): a bucket omitted by ANY in-window
        // increment makes the folded rate bucket Unknown, even when other
        // increments are Known. The `*_unknown` latches keep the
        // accumulator distinct from "no increments" (handled above) and from
        // a sum of only the known intervals.
        match increment.input {
            Some(value) if !input_unknown => {
                input = Some(input.unwrap_or(0).saturating_add(value));
            }
            None => input_unknown = true,
            Some(_) => {}
        }
        match increment.output {
            Some(value) if !output_unknown => {
                output = Some(output.unwrap_or(0).saturating_add(value));
            }
            None => output_unknown = true,
            Some(_) => {}
        }
    }
    if input_unknown {
        input = None;
    }
    if output_unknown {
        output = None;
    }
    Ok((input, output))
}

type LatestStockPoint = (Option<i64>, Option<i64>, Option<i64>, Option<i64>, String);

#[derive(Debug, Default, Clone)]
struct SessionStock {
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    native_observed_at: Option<String>,
    latest_observed_at: Option<String>,
}

fn latest_session_stock(
    conn: &Connection,
    instance_id: &str,
) -> rusqlite::Result<(SessionStock, bool)> {
    // Newest point per scoped session id for counters, in ACCEPTANCE order
    // (insertion rowid, c-ctxusage r10 item 3): the current stock is the last
    // accepted cumulative point, not the point with the newest mixed clock —
    // a late-ingested buffered native point with an earlier native timestamp
    // must not demote the stock. The timestamp source travels with the point
    // so an ingest fallback can never be mistaken for a native session time
    // (c-ctxusage r8 item 9).
    let mut stmt = conn.prepare(
        "WITH ranked AS (
            SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                   observed_at, scope_id, rowid,
                   ROW_NUMBER() OVER (
                       PARTITION BY scope_id
                       ORDER BY rowid DESC
                   ) AS rn
            FROM usage_events
            WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NOT NULL
         )
         SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, observed_at
         FROM ranked WHERE rn = 1
         ORDER BY rowid DESC",
    )?;
    let points: Vec<LatestStockPoint> = stmt
        .query_map(params![instance_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some((first, rest)) = points.split_first() {
        let mut stock = SessionStock::default();
        // The FIRST point seeds every bucket: an empty accumulator must be
        // distinguished from one holding an unknown contribution, so a single
        // session whose latest point knows input reports a known input even
        // before any fold. Later sessions fold under U1 — an unknown bucket in
        // ANY constituent session makes the cross-session total Unknown
        // (c-ctxusage r17 item 6).
        let (input, output, cache_read, cache_write, observed_at) = first;
        stock.input = *input;
        stock.output = *output;
        stock.cache_read = *cache_read;
        stock.cache_write = *cache_write;
        stock.latest_observed_at = Some(observed_at.clone());
        for (input, output, cache_read, cache_write, observed_at) in rest {
            stock.input = add_optional(stock.input, *input);
            stock.output = add_optional(stock.output, *output);
            stock.cache_read = add_optional(stock.cache_read, *cache_read);
            stock.cache_write = add_optional(stock.cache_write, *cache_write);
            if stock.latest_observed_at.as_deref() < Some(observed_at.as_str()) {
                stock.latest_observed_at = Some(observed_at.clone());
            }
        }
        // Trustworthy freshness signal, independent of the newest-by-any-time
        // counters point: the newest NATIVE session time across sessions.
        stock.native_observed_at = conn
            .query_row(
                "SELECT MAX(observed_at) FROM usage_events
                 WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NOT NULL
                   AND observed_at_source = 'native'",
                params![instance_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or(None);
        return Ok((stock, true));
    }
    // Legacy fallback: the last ACCEPTED NULL-scope row (acceptance order,
    // r10 item 3). These rows carry no 'native' source (they are 'ingest' or
    // the migration-default 'legacy'), so native_observed_at stays None — a
    // legacy cumulative total cannot author a lastTurnAt.
    let stock = conn
        .query_row(
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, observed_at
             FROM usage_events
             WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NULL
             ORDER BY rowid DESC LIMIT 1",
            params![instance_id],
            |r| {
                Ok(SessionStock {
                    input: r.get(0)?,
                    output: r.get(1)?,
                    cache_read: r.get(2)?,
                    cache_write: r.get(3)?,
                    native_observed_at: None,
                    latest_observed_at: Some(r.get(4)?),
                })
            },
        )
        .unwrap_or_default();
    Ok((stock, false))
}

fn add_optional(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        _ => None,
    }
}

fn latest_call_basket(conn: &Connection, instance_id: &str) -> rusqlite::Result<Option<i64>> {
    let ctes = fold_ctes_instance();
    let basket_sql = format!(
        "WITH {ctes},
                 baskets AS (
                    SELECT input_tokens, cache_read_tokens, cache_write_tokens,
                           observed_at, seq FROM flow_turns
                    UNION ALL
                    SELECT input_tokens, cache_read_tokens, cache_write_tokens,
                           observed_at, seq FROM flow_messages
                 )
                 SELECT input_tokens, cache_read_tokens, cache_write_tokens
                 FROM baskets
                 ORDER BY observed_at DESC, seq DESC LIMIT 1"
    );
    let Some((input, cache_read, cache_write)) = conn
        .prepare_cached(&basket_sql)?
        .query_row(params![instance_id], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .ok()
    else {
        return Ok(None);
    };
    let (Some(input), Some(cache_read), Some(cache_write)) = (input, cache_read, cache_write)
    else {
        return Ok(None);
    };
    Ok(Some(
        input.saturating_add(cache_read).saturating_add(cache_write),
    ))
}

fn known_context_total(baskets: [Option<i64>; 3]) -> Option<i64> {
    baskets
        .into_iter()
        .collect::<Option<Vec<i64>>>()
        .map(|known| known.into_iter().fold(0, i64::saturating_add))
}

fn latest_native_window(conn: &Connection, instance_id: &str) -> rusqlite::Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT native_context_window FROM usage_events
         WHERE instance_id = ?1 AND native_context_window IS NOT NULL
         ORDER BY observed_at DESC, seq DESC LIMIT 1",
            params![instance_id],
            |row| row.get(0),
        )
        .ok()
        .flatten())
}

pub struct RollupRequest<'a> {
    pub instance_id: &'a str,
    pub kind: &'a str,
    pub spec_model: Option<&'a str>,
    pub effective_model: Option<&'a str>,
    pub profile_id: Option<&'a str>,
}

fn kind_context_window(kind: &str) -> Option<i64> {
    match kind {
        "claude" | "codex" | "agy" => Some(200_000),
        "grok" => Some(128_000),
        _ => None,
    }
}

fn is_one_million_tag(model: &str) -> bool {
    model.trim().to_ascii_lowercase().ends_with("[1m]")
}

fn catalog_window(model: &str) -> Option<i64> {
    crate::model_catalog::lookup(model.trim()).map(|row| row.context_window as i64)
}

fn profile_context_window(
    conn: &Connection,
    profile_id: &str,
    model_id: &str,
) -> rusqlite::Result<Option<i64>> {
    let raw: Option<String> = conn.query_row(
        "SELECT models_json FROM provider_profiles WHERE id = ?1",
        params![profile_id],
        |row| row.get(0),
    )?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let models = crate::provider_models::parse_models_json(&raw);
    let wanted = model_id.trim();
    // A `[1m]` spelling on the profile wins on exact match or the untagged id.
    let entry = models
        .iter()
        .filter(|entry| entry.enabled)
        .find(|entry| entry.id == wanted || entry.id.trim_end_matches("[1m]").trim() == wanted)
        .map(std::borrow::ToOwned::to_owned)
        .or_else(|| {
            models
                .iter()
                .filter(|entry| entry.enabled)
                .find(|entry| {
                    window::tag_matches(entry.id.as_str(), wanted)
                        || entry.id.trim_end_matches("[1m]").trim() == wanted
                })
                .cloned()
        });
    Ok(entry
        .and_then(|entry| entry.context_window)
        .map(|window| window as i64))
}

mod window {
    pub(super) fn tag_matches(entry_id: &str, wanted: &str) -> bool {
        let no_tag = wanted.trim_end_matches("[1m]").trim();
        super::is_one_million_tag(entry_id)
            && (entry_id == wanted
                || entry_id.ends_with(&format!("/{wanted}"))
                || entry_id.ends_with(&format!("/{no_tag}")))
    }
}

fn resolve_context_window(
    conn: &Connection,
    req: &RollupRequest<'_>,
    native_window: Option<i64>,
) -> (Option<i64>, bool) {
    if let Some(window) = native_window {
        return (Some(window), false);
    }
    let Some(model) = req.effective_model.or(req.spec_model) else {
        return kind_fallback(req.kind);
    };
    // 1. explicit [1m] tag.
    if is_one_million_tag(model) {
        return (Some(1_000_000), false);
    }
    // 2. the provider profile's declared window.
    if let Some(profile_id) = req.profile_id
        && let Ok(Some(window)) = profile_context_window(conn, profile_id, model)
    {
        return (Some(window), false);
    }
    // 3. the static catalog (the [1m] tag inside it was handled first).
    if let Some(window) = catalog_window(model) {
        return (Some(window), false);
    }
    kind_fallback(req.kind)
}

fn kind_fallback(kind: &str) -> (Option<i64>, bool) {
    match kind_context_window(kind) {
        Some(window) => (Some(window), true),
        None => (None, false),
    }
}

struct RollupTotals {
    turns: i64,
    turn_input: Option<i64>,
    turn_output: Option<i64>,
    turn_cache_read: Option<i64>,
    turn_cache_write: Option<i64>,
    has_session: bool,
    last_turn_at: Option<String>,
}

pub fn rollup_instance(
    conn: &Connection,
    req: &RollupRequest<'_>,
) -> rusqlite::Result<Option<InstanceUsageRollup>> {
    // c-usagefu r15 item 1: the rollup SQL references final-schema columns
    // (scope_id/turn_id/observed_at_source). c-usagefu r16 item 7: on a
    // database the usage migration could not upgrade it returns an explicit
    // Unavailable rollup (`Some` with `unavailable: true`, every counter None)
    // instead of the `None` "no channel" state — the instance list/detail load
    // succeeds and the client renders Unknown rather than a guessed percentage.
    if !usage_schema_is_current(conn)? {
        return Ok(Some(InstanceUsageRollup::unavailable()));
    }
    let instance_id = req.instance_id;
    // Ruling X fast path: with NO durable usage for this instance the fold
    // below yields no turns, no session and zero rows — exactly the `None`
    // "no channel" result it computes further down. Resolve that with one
    // indexed EXISTS instead of planning the ~41 KB fold graph. This also makes
    // the one-time (~115 ms) statement-plan cost lazy: instances that have not
    // emitted usage (the common case right after create) never plan it, and the
    // graph is cached via `prepare_cached` once an instance does have data.
    let has_any_usage: bool = conn
        .prepare_cached("SELECT 1 FROM usage_events WHERE instance_id = ?1 LIMIT 1")?
        .exists(params![instance_id])?;
    if !has_any_usage {
        return Ok(None);
    }
    // Totals come from the SAME unified fold as the aggregates
    // ([`unified_fold_ctes`]): one winning turn per scope id across lineages,
    // eligible durable Messages, per-session stock-vs-flow composition, and
    // U1 / ambiguity Unknown gating. Legacy NULL-scope Turns count as
    // individual winning rows.
    let ctes = fold_ctes_instance();
    let totals_sql = format!(
        "WITH {ctes}
             SELECT
                -- Turn identities: every winning turn row (a covered turn is
                -- still an observed turn), plus one per eligible durable
                -- Message whose turn has no winning Turn row, plus legacy
                -- turn_id-less Messages only when the instance has no Turn at
                -- all (c-usagefu r7 item 8).
                (SELECT COUNT(*) FROM global_winning_turns
                 WHERE instance_id = ?1)
                + (SELECT COUNT(*) FROM (
                    SELECT DISTINCT fm.turn_id FROM flow_messages fm
                    WHERE fm.turn_id IS NOT NULL
                      AND NOT EXISTS (
                          SELECT 1 FROM global_winning_turns wt
                          WHERE wt.instance_id = fm.instance_id
                            AND wt.scope_id = fm.turn_id
                      )
                    UNION ALL
                    SELECT DISTINCT fm.turn_id FROM flow_messages fm
                    WHERE fm.turn_id IS NULL
                      AND NOT EXISTS (
                          SELECT 1 FROM global_winning_turns wt
                          WHERE wt.instance_id = fm.instance_id
                      )
                  )),
                EXISTS (
                    SELECT 1 FROM usage_events
                    WHERE instance_id = ?1 AND scope = 'session'
                ),
                -- r43 AC3: the aggregate's shared bucket logic — an empty
                -- Turn/stock half is NULL unless a Message contributes, never
                -- a fabricated zero (bill-only Session => Unknown).
                {input},
                {output},
                {cache_read},
                {cache_write},
                (SELECT MAX(observed_at) FROM (
                    SELECT observed_at FROM flow_turns
                     WHERE observed_at_source = 'native'
                    UNION ALL
                    SELECT observed_at FROM flow_messages
                     WHERE observed_at_source = 'native'
                 ))",
        input = token_bucket_expr("input_tokens"),
        output = token_bucket_expr("output_tokens"),
        cache_read = token_bucket_expr("cache_read_tokens"),
        cache_write = token_bucket_expr("cache_write_tokens"),
    );
    let mut totals_stmt = conn.prepare_cached(&totals_sql)?;
    let totals = totals_stmt.query_row(params![instance_id], |row| {
        let turns: i64 = row.get(0)?;
        Ok(RollupTotals {
            turns,
            turn_input: row.get(2)?,
            turn_output: row.get(3)?,
            turn_cache_read: row.get(4)?,
            turn_cache_write: row.get(5)?,
            has_session: row.get(1)?,
            last_turn_at: row.get(6)?,
        })
    })?;
    // A row exists if there is any flow (turn/message) OR any session row.
    let total_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM usage_events WHERE instance_id = ?1",
        params![instance_id],
        |row| row.get(0),
    )?;
    if totals.turns == 0 && !totals.has_session && total_rows == 0 {
        return Ok(None);
    }

    // The cumulative stock exists only for session-emitting harnesses. It is
    // the context-basket fallback only (token totals come from the fold).
    let (stock, _stock_scoped) = if totals.has_session {
        latest_session_stock(conn, instance_id)?
    } else {
        (SessionStock::default(), false)
    };

    let (session_input, session_output, cache_read, cache_creation) = (
        totals.turn_input,
        totals.turn_output,
        totals.turn_cache_read,
        totals.turn_cache_write,
    );

    // Context basket: the newest surviving flow call (U1: Unknown unless that
    // call knew every basket); for a stock-only harness the current stock
    // counters under the same all-known rule.
    let context_used_tokens = latest_call_basket(conn, instance_id)?
        .or_else(|| known_context_total([stock.input, stock.cache_read, stock.cache_write]));

    let native_window = latest_native_window(conn, instance_id)?;
    let (context_window_tokens, window_approximate) =
        resolve_context_window(conn, req, native_window);
    let context_pct = context_used_tokens
        .zip(context_window_tokens)
        .map(|(used, window)| {
            (used as f64 / window as f64 * 100.0)
                .round()
                .clamp(0.0, 100.0) as i64
        });
    let context_pct_approximate = context_pct.is_some() && window_approximate;

    // Rate windows are summed from NATIVE-timestamped flow rows that SURVIVE
    // the fold (an ingest-stamped replay or a lineage-ambiguous/covered turn
    // must not read as a burst). The route depends on whether such a row
    // EXISTS, not on raw has_turn: a covered legacy Turn makes has_turn true
    // but contributes nothing, so an instance whose cumulative Session growth
    // points can still supply increments must take the stock route (r8
    // item 5 / c-ctxusage r28 coverage). The probe is per-INSTANCE: every
    // branch is domain-scoped to ?1 (flow_messages already is), so another
    // instance's native turn can never pick the turn route here (r37 item 6).
    // The ambiguity exclusion is the same rowid-exact one the real rate query
    // in sum_turn_tokens_since uses.
    let fold_ctes = fold_ctes_instance();
    let route_covered = turn_fully_covered_sql("t.instance_id", "t.scope_id");
    let native_flow_sql = format!(
        "WITH {fold_ctes}
             SELECT EXISTS (
                 SELECT 1 FROM global_winning_turns t
                 WHERE t.instance_id = ?1
                   AND t.observed_at_source = 'native'
                   AND NOT ({route_covered})
                   AND NOT EXISTS (
                       SELECT 1 FROM g_ambiguous_turns a
                       WHERE a.winning_rowid = t.winning_rowid
                   )
                 UNION ALL
                 SELECT 1 FROM flow_messages
                 WHERE observed_at_source = 'native'
             )"
    );
    let mut native_flow_stmt = conn.prepare_cached(&native_flow_sql)?;
    let has_native_flow: bool =
        native_flow_stmt.query_row(params![instance_id], |row| row.get(0))?;

    // Rate windows: per-turn flows for harnesses that emit them; otherwise
    // timestamped increments between cumulative session snapshots (r4 item 6).
    // A single stock with no predecessor yields no trustworthy rate.
    let window = |seconds: i64| -> rusqlite::Result<(Option<i64>, Option<i64>)> {
        let since = threshold_rfc3339(seconds);
        if has_native_flow {
            sum_turn_tokens_since(conn, instance_id, &since)
        } else {
            sum_session_increments_since(conn, instance_id, &since)
        }
    };
    let (in_60s, out_60s) = window(60)?;
    let (in_5m, out_5m) = window(300)?;
    // The 5-minute figure is an average per-minute rate (sum / 5).
    let per_minute_5m =
        |total: Option<i64>| -> Option<i64> { total.map(|n| (n as f64 / 5.0).round() as i64) };

    // c-ctxusage r8 item 9: only a NATIVE session time may author lastTurnAt;
    // an ingest fallback stamped at replay time is not session activity.
    let last_turn_at = totals.last_turn_at.or(stock.native_observed_at);

    Ok(Some(InstanceUsageRollup {
        unavailable: false,
        context_used_tokens,
        context_window_tokens,
        context_pct,
        context_pct_approximate,
        session_input_tokens: session_input,
        session_output_tokens: session_output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        turns: totals.turns,
        tpm_in_60s: in_60s,
        tpm_out_60s: out_60s,
        tpm_in_5m: per_minute_5m(in_5m),
        tpm_out_5m: per_minute_5m(out_5m),
        last_turn_at,
    }))
}

impl InstanceUsageRollup {
    fn unavailable() -> Self {
        Self {
            unavailable: true,
            context_used_tokens: None,
            context_window_tokens: None,
            context_pct: None,
            context_pct_approximate: false,
            session_input_tokens: None,
            session_output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            turns: 0,
            tpm_in_60s: None,
            tpm_out_60s: None,
            tpm_in_5m: None,
            tpm_out_5m: None,
            last_turn_at: None,
        }
    }
}

pub async fn observe_journal(state: &AppState, record: &JournalRecord) {
    if record.event.get("kind").and_then(Value::as_str) != Some("usage") {
        return;
    }
    let instance = state
        .store
        .get_instance(record.instance_id.clone())
        .await
        .ok()
        .flatten();
    let profile_id = instance
        .as_ref()
        .and_then(|i| i.provider_profile_id.clone());
    let model = instance.as_ref().and_then(|i| i.model.clone());
    let Some(row) = project_usage_event(record, profile_id.as_deref(), model.as_deref()) else {
        return;
    };
    let inserted = state
        .store
        .run_named("observe_journal", move |conn| {
            insert_usage_event(conn, &row).map_err(crate::store::StoreError::from)
        })
        .await;
    if let Err(error) = inserted {
        tracing::warn!(%error, instance_id = %record.instance_id, "usage event projection failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::JournalRecord;
    use remuda_driver::adapters::FileSignalAdapter;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn usage_record(seq: i64, total: u64, cost: Option<&str>) -> JournalRecord {
        JournalRecord {
            instance_id: "ins_test".into(),
            seq,
            event_id: format!("evt_{seq}"),
            event: json!({
                "kind": "usage",
                "nativeTurnId": { "state": "known", "value": format!("turn-{seq}") },
                "payload": {
                    "scope": "turn",
                    "scopeId": format!("turn-{seq}"),
                    "mode": "snapshot",
                    "totalTokens": { "state": "known", "value": total.to_string() },
                    "inputTokens": { "state": "known", "value": (total / 2).to_string() },
                    "outputTokens": { "state": "known", "value": (total / 2).to_string() },
                    "cost": cost.map_or(
                        json!({ "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] }),
                        |amount| json!({
                            "state": "known",
                            "value": { "amount": amount, "currency": "USD" }
                        })
                    ),
                    "accounting": "estimated"
                }
            }),
            observed_at: "2026-09-15T00:00:00.000Z".into(),
        }
    }

    fn row(
        seq: i64,
        observed_at: &str,
        input: Option<i64>,
        output: Option<i64>,
        cache_read: Option<i64>,
        cache_write: Option<i64>,
    ) -> UsageEventRow {
        UsageEventRow {
            instance_id: "ins_test".into(),
            seq,
            profile_id: None,
            model: None,
            scope: "turn".into(),
            scope_id: None,
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: None,
            total_tokens: None,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: observed_at.into(),
            observed_at_source: "native".into(),
        }
    }

    fn scope_id_at(conn: &Connection, seq: i64) -> Option<String> {
        conn.query_row(
            "SELECT scope_id FROM usage_events WHERE seq = ?1 AND scope = 'session'",
            params![seq],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn legacy_session_row(
        seq: i64,
        instance: &str,
        input: i64,
        output: i64,
        at: &str,
    ) -> UsageEventRow {
        UsageEventRow {
            instance_id: instance.into(),
            seq,
            profile_id: None,
            model: Some("codex".into()),
            scope: "session".into(),
            scope_id: None,
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: None,
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: at.into(),
            observed_at_source: "native".into(),
        }
    }

    fn open_old_dual_schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                scope_id TEXT,
                turn_id TEXT,
                mode TEXT NOT NULL,
                metric_revision INTEGER NOT NULL DEFAULT 1,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                observed_at_source TEXT NOT NULL DEFAULT 'ingest',
                native_context_window INTEGER,
                PRIMARY KEY (instance_id, seq)
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn legacy_message_without_turn_id_bills_the_covering_turn_once() {
        // c-usagefu r10 item 4 (Ruling B): a pre-r6 codex call wrote a Message
        // r1 = 100 with NO turn_id AND a Turn t1 = 100 covering its time
        // window (the message is recorded 3 s before the end-of-turn snapshot).
        // The call must count 100, not 200: the link-less legacy Message is
        // dropped in favour of the covering Turn. A separate Turn-only call t2
        // = 50 with no Message still counts, proving turn rows are not broadly
        // suppressed.
        let conn = open_old_dual_schema();
        conn.execute_batch(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 output_tokens, cost_usd, observed_at, observed_at_source,
                 profile_id, model)
             VALUES
                ('ins_legacy', 1, 'message', 'r1', NULL, 'snapshot',
                 100, '0.10', '2030-01-01T00:00:00.000Z', 'native',
                 'pvp_legacy', 'gpt-5.4'),
                ('ins_legacy', 2, 'turn', 't1', 't1', 'snapshot',
                 100, '0.10', '2030-01-01T00:00:03.000Z', 'native',
                 'pvp_legacy', 'gpt-5.4'),
                ('ins_legacy', 3, 'turn', 't2', 't2', 'snapshot',
                 50, '0.05', '2030-01-01T00:00:07.000Z', 'native',
                 'pvp_legacy', 'gpt-5.4');",
        )
        .unwrap();
        migrate(&conn).unwrap();

        let agg = aggregate_instance(&conn, "ins_legacy").unwrap();
        assert_eq!(
            agg.output_tokens,
            Some(150),
            "covering Turn 100 (legacy Message dropped) + Turn-only 50"
        );
        assert!(
            (agg.cost_usd.unwrap() - 0.15).abs() < 1e-9,
            "0.10 + 0.05, not 0.25"
        );
        let supply = aggregate_supply(&conn, "pvp_legacy", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, Some(150));
        assert!((supply.cost_usd.unwrap() - 0.15).abs() < 1e-9);
        let rollup = rollup_fold(&conn, "ins_legacy", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(150));
    }

    #[test]
    fn legacy_messages_are_dropped_when_the_instance_has_any_turn_even_hours_away() {
        // c-usagefu r11 item 6 (Ruling B1, no clocks): pre-r6 codex emitted a
        // Turn for EVERY call alongside its attribution-less Message, so for an
        // instance that has ANY Turn row a Message without turn_id is NEVER
        // counted — no matter how far its timestamp sits from a Turn. The r10
        // rule kept a legacy Message when no Turn fell inside its ±10 min
        // window, which let an "unrelated"-looking legacy Message on a
        // turn-bearing instance bill a second time.
        //
        // Actual old-schema database, migrated: one Turn at noon and a legacy
        // Message the same evening (five hours later, far outside the old
        // window). Only the Turn counts.
        let conn = open_old_dual_schema();
        conn.execute_batch(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 output_tokens, cost_usd, observed_at, observed_at_source,
                 profile_id, model)
             VALUES
                ('ins_b1_turns', 1, 'turn', 't1', 't1', 'snapshot',
                 100, '0.10', '2030-01-01T12:00:00.000Z', 'native',
                 'pvp_b1', 'gpt-5.4'),
                ('ins_b1_turns', 2, 'message', 'r-late', NULL, 'snapshot',
                 70, '0.07', '2030-01-01T17:00:00.000Z', 'native',
                 'pvp_b1', 'gpt-5.4');",
        )
        .unwrap();
        migrate(&conn).unwrap();

        let agg = aggregate_instance(&conn, "ins_b1_turns").unwrap();
        assert_eq!(
            agg.output_tokens,
            Some(100),
            "Turns only: the five-hours-away legacy Message is NOT billed"
        );
        assert!(
            (agg.cost_usd.unwrap() - 0.10).abs() < 1e-9,
            "the Message cost is dropped too"
        );
        let rollup = rollup_fold(&conn, "ins_b1_turns", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(100));
    }

    #[test]
    fn an_instance_with_no_turns_at_all_counts_every_legacy_message() {
        // c-usagefu r11 item 6 (Ruling B1): the other side of the rule. An
        // instance whose old-schema database holds ONLY link-less legacy
        // Messages (no Turn rows whatsoever) counts ALL of them, whatever their
        // timestamps — there is no time window that could drop one. Two
        // Messages a day apart both bill.
        let conn = open_old_dual_schema();
        conn.execute_batch(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 output_tokens, cost_usd, observed_at, observed_at_source,
                 profile_id, model)
             VALUES
                ('ins_b1_msgs', 1, 'message', 'r1', NULL, 'snapshot',
                 40, '0.04', '2030-01-01T08:00:00.000Z', 'native',
                 'pvp_b1m', 'gpt-5.4'),
                ('ins_b1_msgs', 2, 'message', 'r2', NULL, 'snapshot',
                 60, '0.06', '2030-01-02T08:00:00.000Z', 'native',
                 'pvp_b1m', 'gpt-5.4');",
        )
        .unwrap();
        migrate(&conn).unwrap();

        let agg = aggregate_instance(&conn, "ins_b1_msgs").unwrap();
        assert_eq!(
            agg.output_tokens,
            Some(100),
            "a turn-less instance counts both legacy Messages, a day apart"
        );
        assert!((agg.cost_usd.unwrap() - 0.10).abs() < 1e-9);
        let supply = aggregate_supply(&conn, "pvp_b1m", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, Some(100));
        let rollup = rollup_fold(&conn, "ins_b1_msgs", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(100));
    }

    #[test]
    fn partially_covered_turn_bills_the_turn_and_a_fully_covered_one_bills_messages() {
        // c-usagefu r13 item 2 (Ruling U1): a Turn is suppressed in favour of
        // its Messages only when its DURABLE linked Messages match it on every
        // token bucket (bucket-sum equality).
        //   t1: Turn 120 / $0.12 with ONE linked Message r1 = 100 / $0.10
        //       under a non-durable identity (the other accepted 20 record was
        //       never emitted as a Message) -> the durable sum is 0, not 120:
        //       mismatch, count the Turn 120 and drop r1 (never 100, never 220).
        //   t2: Turn 120 / $0.12 fully covered by r2a = 100 plus r2b = 20,
        //       BOTH durable -> bill the two Messages, drop the Turn.
        // Both turns contribute 120 -> 240 output, $0.24.
        use remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX as M;
        let r2a = format!("{M}100:rollout-partial");
        let r2b = format!("{M}260:rollout-partial");
        let conn = open_old_dual_schema();
        conn.execute_batch(&format!(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 output_tokens, cost_usd, observed_at, observed_at_source,
                 profile_id, model)
             VALUES
                ('ins_partial', 1, 'message', 'r1', 't1', 'snapshot',
                 100, '0.10', '2030-01-01T00:00:00.000Z', 'native',
                 'pvp_partial', 'gpt-5.4'),
                ('ins_partial', 2, 'turn', 't1', 't1', 'snapshot',
                 120, '0.12', '2030-01-01T00:00:01.000Z', 'native',
                 'pvp_partial', 'gpt-5.4'),
                ('ins_partial', 3, 'message', '{r2a}', 't2', 'snapshot',
                 100, '0.10', '2030-01-01T00:00:04.000Z', 'native',
                 'pvp_partial', 'gpt-5.4'),
                ('ins_partial', 4, 'message', '{r2b}', 't2', 'snapshot',
                 20, '0.02', '2030-01-01T00:00:04.500Z', 'native',
                 'pvp_partial', 'gpt-5.4'),
                ('ins_partial', 5, 'turn', 't2', 't2', 'snapshot',
                 120, '0.12', '2030-01-01T00:00:05.000Z', 'native',
                 'pvp_partial', 'gpt-5.4');",
        ))
        .unwrap();
        migrate(&conn).unwrap();

        // Per-row eligibility — the totals alone cannot tell a covered Turn
        // from a billed one (120 == 100 + 20 either way): t1's Turn counts
        // and its legacy Message drops; t2's Turn drops and BOTH durable
        // Messages count.
        let eligible = |scope: &str, scope_id: &str| -> bool {
            conn.query_row(
                &format!(
                    "SELECT ({pred}) FROM usage_events
                     WHERE instance_id='ins_partial' AND scope=?1 AND scope_id=?2",
                    pred = eligible_flow_predicate("usage_events")
                ),
                params![scope, scope_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
                == 1
        };
        assert!(eligible("turn", "t1"), "t1 Turn is billed");
        assert!(!eligible("message", "r1"), "t1's legacy Message is dropped");
        assert!(!eligible("turn", "t2"), "t2 Turn is suppressed");
        assert!(eligible("message", &r2a), "t2 durable Message a counts");
        assert!(eligible("message", &r2b), "t2 durable Message b counts");

        let agg = aggregate_instance(&conn, "ins_partial").unwrap();
        assert_eq!(
            agg.output_tokens,
            Some(240),
            "partial t1 counts its Turn 120; fully covered t2 counts 100+20"
        );
        assert!((agg.cost_usd.unwrap() - 0.24).abs() < 1e-9, "0.12 + 0.12");
        let supply = aggregate_supply(&conn, "pvp_partial", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, Some(240));
        assert!((supply.cost_usd.unwrap() - 0.24).abs() < 1e-9);
        let rollup = rollup_fold(&conn, "ins_partial", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(240));
    }

    #[test]
    fn bucket_sum_equality_decides_turn_suppression_and_a_no_turn_turn_counts_its_messages() {
        // c-usagefu r13 item 2 (COORDINATOR RULING U1): the r12 identity-MIX
        // predicate reconciled only shapes the intermediate unlanded rounds
        // could produce, and U1 puts those out of scope. Within in-scope data
        // bucket-sum equality between a Turn row and its DURABLE linked
        // Messages IS the completeness proof; a mismatch counts the Turn and
        // drops the Messages, and a turn with no Turn row counts its linked
        // Messages. Actual old-schema database, migrated, five turns:
        //
        //  t1 Turn 120 + linked Messages: a non-durable record 100 (stale-priced
        //     0.99) and a DURABLE record 20 (0.02) -> the DURABLE sum (20) does
        //     not match the Turn (120): bill the Turn 120 / 0.12, drop both
        //     Messages (never the stale 0.99, never 140).
        //  t2 Turn 120 + two DURABLE Messages 100+20 (0.10+0.02) -> sums match
        //     bucket-for-bucket: suppress the Turn, bill the Messages.
        //  t3 NO Turn row: a non-durable 50 and a DURABLE 50 -> U1 deletes the
        //     mixed-identity drop; a turn without a Turn row counts its linked
        //     Messages, so both bill (100). (The r12 assertion of a ZERO
        //     contribution for this mixed no-Turn turn is removed by U1.)
        //  t4 NO Turn row, one non-durable Message 30 -> counts as before.
        //  t5 NO Turn row, one DURABLE Message 70 -> final-shape turn counts.
        //
        // Output: 120 + 120 + 100 + 30 + 70 = 440.
        use remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX as M;
        let d = |off: u64| format!("{M}{off}:rollout-r12-item4");
        let conn = open_old_dual_schema();
        conn.execute_batch(&format!(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 output_tokens, cost_usd, observed_at, observed_at_source,
                 profile_id, model)
             VALUES
                ('ins_durable', 1, 'message', 'legacy-r1', 't1', 'snapshot',
                 100, '0.99', '2030-01-01T00:00:00.000Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 2, 'message', '{d1b}', 't1', 'snapshot',
                 20, '0.02', '2030-01-01T00:00:00.200Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 3, 'turn', 't1', 't1', 'snapshot',
                 120, '0.12', '2030-01-01T00:00:01.000Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 4, 'message', '{d2a}', 't2', 'snapshot',
                 100, '0.10', '2030-01-01T00:00:02.000Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 5, 'message', '{d2b}', 't2', 'snapshot',
                 20, '0.02', '2030-01-01T00:00:02.200Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 6, 'turn', 't2', 't2', 'snapshot',
                 120, '0.12', '2030-01-01T00:00:03.000Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 7, 'message', 'legacy-r3', 't3', 'snapshot',
                 50, '0.05', '2030-01-01T00:00:04.000Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 8, 'message', '{d3b}', 't3', 'snapshot',
                 50, '0.05', '2030-01-01T00:00:04.200Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 9, 'message', 'legacy-r4', 't4', 'snapshot',
                 30, '0.03', '2030-01-01T00:00:05.000Z', 'native',
                 'pvp_d', 'gpt-5.4'),
                ('ins_durable', 10, 'message', '{d5}', 't5', 'snapshot',
                 70, '0.07', '2030-01-01T00:00:06.000Z', 'native',
                 'pvp_d', 'gpt-5.4');",
            d1b = d(300),
            d2a = d(400),
            d2b = d(520),
            d3b = d(640),
            d5 = d(760),
        ))
        .unwrap();
        migrate(&conn).unwrap();

        let eligible = |scope: &str, scope_id: &str| -> bool {
            conn.query_row(
                &format!(
                    "SELECT ({pred}) FROM usage_events
                     WHERE instance_id='ins_durable' AND scope=?1 AND scope_id=?2",
                    pred = eligible_flow_predicate("usage_events")
                ),
                params![scope, scope_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
                == 1
        };
        // t1: the durable sum mismatches the Turn -> the Turn bills and both
        // linked Messages drop.
        assert!(eligible("turn", "t1"), "mismatched t1 bills the Turn");
        assert!(!eligible("message", "legacy-r1"));
        assert!(!eligible("message", &d(300)));
        // t2: durable sums match -> Messages bill, Turn suppresses.
        assert!(!eligible("turn", "t2"), "fully covered t2 suppresses Turn");
        assert!(eligible("message", &d(400)));
        assert!(eligible("message", &d(520)));
        // t3: no Turn row -> every linked Message counts (U1; the r12 mixed
        // no-Turn zero contribution is gone).
        assert!(
            eligible("message", "legacy-r3"),
            "no-Turn t3 counts its Messages"
        );
        assert!(eligible("message", &d(640)));
        // t4/t5: no-Turn turns count.
        assert!(
            eligible("message", "legacy-r4"),
            "non-durable no-Turn turn counts"
        );
        assert!(eligible("message", &d(760)), "durable no-Turn turn counts");

        let agg = aggregate_instance(&conn, "ins_durable").unwrap();
        assert_eq!(
            agg.output_tokens,
            Some(440),
            "120 t1 Turn + 120 t2 Messages + 100 t3 Messages + 30 + 70"
        );
        assert!(
            (agg.cost_usd.unwrap() - 0.44).abs() < 1e-9,
            "0.12 + 0.12 + 0.10 + 0.03 + 0.07"
        );
    }

    #[test]
    fn migration_upgrades_a_pre_delivery_database_preserving_rows() {
        // c-ctxusage r2: a database created at the 3bd07311 schema (no
        // cache_* / scope_id columns, and no dedupe index) must open and
        // migrate without losing its existing rows.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                mode TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );",
        )
        .unwrap();
        // r8 item 5: a second, pre-`scopeId` CODEX-style instance — its old
        // rows are NULL-scope Turn flow AND cumulative Session growth points
        // (the pre-r6 codex shape). This shape predates the source column, so
        // migration defaults its rows to 'legacy' and TPM still comes from the
        // session-stock increments, not be blanked by a native-only turn query
        // that has_turn routes to. Points: 100/60 twenty seconds ago, 160/84
        // two seconds ago (increment 60/24 inside both windows); a legacy turn
        // row at 30s.
        let at_30s = threshold_rfc3339(30);
        let at_20s = threshold_rfc3339(20);
        let at_2s = threshold_rfc3339(2);
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, mode, input_tokens, output_tokens,
                 accounting, observed_at)
             VALUES
                ('ins_old_cx', 1, 'turn', 'snapshot', 40, 30,
                 'estimated', ?1),
                ('ins_old_cx', 2, 'session', 'snapshot', 100, 60,
                 'estimated', ?2),
                ('ins_old_cx', 3, 'session', 'snapshot', 160, 84,
                 'estimated', ?3)",
            params![at_30s, at_20s, at_2s],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, mode, total_tokens, input_tokens,
                 output_tokens, accounting, observed_at)
             VALUES ('ins_old', 1, 'turn', 'snapshot', 1000, 500, 500,
                     'estimated', '2026-09-01T00:00:00.000Z')",
            [],
        )
        .unwrap();

        // Runs the full current migration against the old table.
        migrate(&conn).unwrap();

        // New columns exist and the dedupe index is in place.
        let mut stmt = conn.prepare("PRAGMA table_info(usage_events)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for col in ["cache_read_tokens", "cache_write_tokens", "scope_id"] {
            assert!(cols.iter().any(|c| c == col), "column {col} migrated");
        }
        // Old row is preserved (new columns NULL).
        let (total, scope_id): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT total_tokens, scope_id FROM usage_events WHERE instance_id='ins_old'",
                [],
                |row| Ok((row.get(0).unwrap(), row.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(total, Some(1000));
        assert!(
            scope_id.is_none(),
            "old row has no scope_id, so is not deduped"
        );
        // Rollup works after upgrade AND counts the legacy NULL-scope_id Turn
        // row correctly (r7 item 8): one turn, its tokens in totals.
        let upgraded = rollup_fold(&conn, "ins_old", "claude", Some("fake"))
            .unwrap()
            .expect("legacy row rolls up");
        assert_eq!(upgraded.turns, 1, "legacy NULL-scope_id turn is counted");
        assert_eq!(upgraded.session_input_tokens, Some(500));
        assert_eq!(upgraded.session_output_tokens, Some(500));
        // Legacy rows were written before the native/ingest source split;
        // they default to observed_at_source='ingest', so lastTurnAt is not
        // driven from them (r7 item 8 asserts the COUNT fix, not source repair
        // of pre-split rows). This lone row is also outside every rate window.

        // r8 item 5: on the legacy codex-shaped instance the NULL-scope Turn
        // row makes has_turn true, but no flow row is native; rates must still
        // come from the cumulative Session increments (60/24), never be
        // blanked to NULL by the native-only turn query.
        let upgraded_cx = rollup_fold(&conn, "ins_old_cx", "codex", None)
            .unwrap()
            .expect("legacy mixed-shape rows roll up");
        assert_eq!(upgraded_cx.turns, 1, "legacy NULL-scope turn is counted");
        assert_eq!(
            upgraded_cx.tpm_in_60s,
            Some(60),
            "session increment supplies the rate despite has_turn"
        );
        assert_eq!(upgraded_cx.tpm_out_60s, Some(24));
        assert_eq!(upgraded_cx.tpm_in_5m, Some(12), "60 tokens / 5 minutes");
        assert_eq!(upgraded_cx.tpm_out_5m, Some(5), "24 tokens / 5 rounds to 5");
    }

    #[test]
    fn re_hydration_dedupes_on_instance_scope_scope_id() {
        // c-ctxusage RC1: a re-bound transcript re-emits the same per-message
        // usage snapshot with a NEW journal seq. The durable key is
        // (instance_id, scope, scope_id): the second projection is ignored so
        // re-hydration never double-counts.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let record = |seq: i64, scope_id: &str, tokens: u64| {
            let mut rec = usage_record(seq, tokens, Some("0.01"));
            rec.event["payload"]["scopeId"] = json!(scope_id);
            rec
        };
        // First mapper pass: two distinct messages.
        let r1 = project_usage_event(&record(1, "msg-a", 100), None, None).unwrap();
        let r2 = project_usage_event(&record(2, "msg-b", 200), None, None).unwrap();
        assert!(insert_usage_event(&conn, &r1).unwrap());
        assert!(insert_usage_event(&conn, &r2).unwrap());
        // A fresh mapper re-hydrates the file: new seqs, same scope ids.
        let r1b = project_usage_event(&record(3, "msg-a", 100), None, None).unwrap();
        let r2b = project_usage_event(&record(4, "msg-b", 200), None, None).unwrap();
        assert!(
            !insert_usage_event(&conn, &r1b).unwrap(),
            "msg-a duplicate ignored"
        );
        assert!(
            !insert_usage_event(&conn, &r2b).unwrap(),
            "msg-b duplicate ignored"
        );

        let total = aggregate_instance(&conn, "ins_test").unwrap();
        assert_eq!(total.events, 2, "no double counting after re-hydration");
        assert_eq!(
            total.total_tokens,
            Some(300),
            "the original two rows, not four"
        );
    }

    #[test]
    fn projects_folds_and_dedupes_usage_events() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let first = project_usage_event(
            &usage_record(1, 1000, Some("0.01")),
            Some("pvp_relay"),
            Some("gw/model_x[1m]"),
        )
        .unwrap();
        assert_eq!(first.total_tokens, Some(1000));
        assert_eq!(first.cost_usd.as_deref(), Some("0.01"));
        assert!(insert_usage_event(&conn, &first).unwrap());
        // Replay of the same seq is a no-op.
        assert!(!insert_usage_event(&conn, &first).unwrap());
        let second =
            project_usage_event(&usage_record(2, 500, Some("0.02")), Some("pvp_relay"), None)
                .unwrap();
        insert_usage_event(&conn, &second).unwrap();

        let total = aggregate_instance(&conn, "ins_test").unwrap();
        assert_eq!(total.events, 2);
        assert_eq!(total.total_tokens, Some(1500));
        assert_eq!(total.cost_usd, Some(0.03));

        // Unknown-cost events are not projected as cost rows...
        let third = usage_record(3, 10, None);
        let third = project_usage_event(&third, Some("pvp_relay"), Some("gw/model-y[1m]")).unwrap();
        insert_usage_event(&conn, &third).unwrap();
        let supply = aggregate_supply(&conn, "pvp_relay", "gw/model-y[1m]").unwrap();
        assert_eq!(supply.events, 1);
        assert_eq!(
            supply.cost_usd, None,
            "an unpriced row leaves the bill Unknown"
        );

        // Non-usage journal rows project to nothing.
        let mut other = usage_record(4, 1, None);
        other.event["kind"] = json!("message");
        assert!(project_usage_event(&other, None, None).is_none());
    }

    #[test]
    fn reported_session_cost_replaces_the_per_call_estimates_and_keeps_growing() {
        // c-usagefu r3 item 3: a two-turn SDK session on a priced model.
        //  - each per-call Turn row carries the driver's independently PRICED
        //    estimate (0.01, 0.02);
        //  - each result frame emits a token-LESS Session snapshot carrying
        //    the reported CUMULATIVE cost (0.01, then 0.03).
        // estimatedUsd must be the latest reported cumulative (0.03), not
        // Sigma per-call + the frozen turn-1 session cost (0.04).
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Turn 1: per-call estimate + the result's cumulative session point.
        let t1 = usage_record(1, 1000, Some("0.01"));
        insert(&conn, &t1);
        let s1 = tokenless_session_record(2, "sdk-session-1", 1, Some("0.01"));
        assert!(insert(&conn, &s1), "first result appends");

        let after_one = aggregate_instance(&conn, "ins_test").unwrap();
        assert!(
            (after_one.cost_usd.unwrap() - 0.01).abs() < 1e-9,
            "the reported session stock, not the 0.01 turn estimate + 0.01 stock"
        );

        // Turn 2: another per-call estimate and the cumulative result.
        let t2 = usage_record(3, 2000, Some("0.02"));
        insert(&conn, &t2);
        let s2 = tokenless_session_record(4, "sdk-session-1", 2, Some("0.03"));
        assert!(
            insert(&conn, &s2),
            "a token-less snapshot whose cumulative COST grew must append"
        );

        let after_two = aggregate_instance(&conn, "ins_test").unwrap();
        assert!(
            (after_two.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "latest cumulative reported cost, not 0.01+0.02+0.01 (2x/frozen)"
        );

        // A re-delivered turn-2 result (identical counters AND cost) freezes.
        let s2_again = tokenless_session_record(5, "sdk-session-1", 2, Some("0.03"));
        assert!(!insert(&conn, &s2_again), "identical cost+counters frozen");
    }

    #[test]
    fn without_session_cost_the_per_call_estimates_still_sum() {
        // Harnesses that emit only per-call rows (no reported session cost)
        // keep the priced flow sum as the estimate.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        insert(&conn, &usage_record(1, 1000, Some("0.01")));
        insert(&conn, &usage_record(2, 500, Some("0.02")));
        let agg = aggregate_instance(&conn, "ins_test").unwrap();
        assert!((agg.cost_usd.unwrap() - 0.03).abs() < 1e-9);
    }

    #[test]
    fn a_session_point_whose_cost_shrinks_is_frozen() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(insert(
            &conn,
            &tokenless_session_record(1, "s", 1, Some("0.03"))
        ));
        assert!(
            !insert(&conn, &tokenless_session_record(2, "s", 2, Some("0.01"))),
            "cumulative cost is monotonic like the token stock"
        );
        let agg = aggregate_instance(&conn, "ins_test").unwrap();
        assert!((agg.cost_usd.unwrap() - 0.03).abs() < 1e-9);
    }

    #[test]
    fn real_codex_adapter_priced_unpriced_priced_bills_0_30_not_0_60() {
        // c-usagefu r6 item 5: drive a REAL CodexAdapter over a rollout file
        // (one Message usage per priced call + cumulative Session snapshots,
        // NO Turn rows since r6 item 5), project every usage observation, then
        // assert both aggregates report the Message flow sum 0.10 + 0.20 =
        // 0.30 — not the r4 double-stream 0.60 (Message+Turn), nor frozen 0.10.
        use remuda_driver::adapters::{AdapterHome, CodexAdapter};
        use remuda_protocol::ObservationPayload;
        use std::io::Write;

        let home_dir = tempfile::tempdir().unwrap();
        let session_dir = home_dir.path().join(".codex/sessions/2026/09/15");
        std::fs::create_dir_all(&session_dir).unwrap();
        let rollout = session_dir.join("rollout-codex-e2e.jsonl");
        // Header first.
        std::fs::write(
            &rollout,
            r#"{"ordinal":0,"type":"session_meta","payload":{"id":"codex-e2e","session_id":"codex-e2e","cwd":"/w","timestamp":"2026-09-15T00:00:00.000Z"}}
"#,
        )
        .unwrap();

        let mut adapter = CodexAdapter::new(AdapterHome {
            home: home_dir.path().join(".codex"),
            cwd: "/w".into(),
            pid: None,
            launched_at: None,
            resume_session_id: None,
        });
        adapter.bind_rollout("codex-e2e", rollout.clone());

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut seq = 0i64;
        let drain = |adapter: &mut CodexAdapter, conn: &Connection, seq: &mut i64| {
            for observed in adapter.poll().unwrap() {
                let ObservationPayload::Usage(payload) = observed.payload else {
                    continue;
                };
                let rec = JournalRecord {
                    instance_id: "ins_codex_e2e".into(),
                    seq: *seq,
                    event_id: format!("evt_codex_e2e_{}", *seq),
                    event: json!({
                        "kind": "usage",
                        "payload": serde_json::to_value(&payload).unwrap(),
                    }),
                    observed_at: "2026-09-15T00:00:00.000Z".into(),
                };
                let row = project_usage_event(&rec, Some("pvp_codex"), Some("gpt-5.4")).unwrap();
                insert_row(conn, &row, IngestEra::Live).expect("codex usage row projects");
                *seq += 1;
            }
        };

        let append_turn = |turn: &str,
                           response: &str,
                           model: &str,
                           tokens: (u64, u64, u64, u64),
                           at: &str| {
            let (input, cached, write, output) = tokens;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&rollout)
                .unwrap();
            writeln!(
                file,
                "{{\"ordinal\":1,\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"{turn}\",\"model\":\"{model}\"}}}}"
            )
            .unwrap();
            writeln!(
                file,
                "{{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn}\"}}}}"
            )
            .unwrap();
            {
                let total = input + cached + write + output;
                writeln!(
                    file,
                    "{{\"ordinal\":3,\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn}\",\"response_id\":\"{response}\",\"usage\":{{\"input_tokens\":{input},\"cached_input_tokens\":{cached},\"cache_write_input_tokens\":{write},\"output_tokens\":{output},\"reasoning_output_tokens\":0,\"total_tokens\":{total}}}}}}}"
                )
                .unwrap();
            }
            writeln!(
                file,
                "{{\"ordinal\":4,\"timestamp\":\"{at}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{turn}\"}}}}"
            )
            .unwrap();
        };

        // Turn 1: priced gpt-5.4 (1000 in / 100 out).
        append_turn(
            "t1",
            "r1",
            "gpt-5.4",
            (0, 0, 0, 10_000),
            "2026-09-15T00:00:01.000Z",
        );
        drain(&mut adapter, &conn, &mut seq);

        // Turn 2: unknown model -> sticky-Unknown priced cost.
        append_turn(
            "t2",
            "r2",
            "some-internal-model",
            (1000, 0, 0, 100),
            "2026-09-15T00:00:02.000Z",
        );
        drain(&mut adapter, &conn, &mut seq);

        // Turn 3: priced again (2000 in / 200 out).
        append_turn(
            "t3",
            "r3",
            "gpt-5.4",
            (0, 0, 0, 20_000),
            "2026-09-15T00:00:03.000Z",
        );
        drain(&mut adapter, &conn, &mut seq);

        let instance = aggregate_instance(&conn, "ins_codex_e2e").unwrap();
        assert!(
            (instance.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "real codex Message-stream sequence: 0.10 + 0.20 = 0.30, got {}",
            instance.cost_usd.unwrap()
        );
        let supply = aggregate_supply(&conn, "pvp_codex", "gpt-5.4").unwrap();
        assert!(
            (supply.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "supply matches the flow bill, got {}",
            supply.cost_usd.unwrap()
        );
    }

    fn create_main_shaped_usage_tables(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE journal (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_id TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );
             CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                mode TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );",
        )
        .unwrap();
    }

    fn insert_main_turn_with_journal(
        conn: &Connection,
        instance_id: &str,
        seq: i64,
        scope_id_in_journal: &str,
        r: &UsageEventRow,
        at: &str,
    ) {
        conn.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                instance_id,
                seq,
                format!("je-{seq}"),
                json!({"kind":"usage","payload":{"scope":"turn","scopeId":scope_id_in_journal}})
                    .to_string(),
                at,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode,
                 total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd,
                 accounting, observed_at)
             VALUES (?1, ?2, ?3, ?4, 'turn', 'snapshot', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                instance_id,
                seq,
                r.profile_id,
                r.model,
                r.total_tokens,
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens,
                r.cost_usd,
                r.accounting,
                at,
            ],
        )
        .unwrap();
    }

    fn real_codex_turn_counters(
        home: &Path,
        session_id: &str,
        turn: &str,
        input: u64,
        output: u64,
        at: &str,
    ) -> UsageEventRow {
        use remuda_driver::adapters::{
            AdapterHome, CodexAdapter, FileSignalAdapter, StampCtx, stamp,
        };
        use remuda_protocol::ObservationPayload;
        use std::io::Write;
        let codex_home = home.join(".codex");
        let date_dir = codex_home.join("sessions/2026/10/13");
        std::fs::create_dir_all(&date_dir).unwrap();
        let rollout = date_dir.join(format!("rollout-{session_id}.jsonl"));
        let total = input + output;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&rollout)
            .unwrap();
        writeln!(
            file,
            "{{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"session_id\":\"{session_id}\",\"cwd\":\"/w\",\"timestamp\":\"2026-10-13T00:00:00.000Z\"}}}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"ordinal\":1,\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"{turn}\",\"model\":\"gpt-5.4\"}}}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn}\"}}}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"ordinal\":3,\"timestamp\":\"{at}\",\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn}\",\"response_id\":\"{turn}-r\",\"usage\":{{\"input_tokens\":{input},\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":{output},\"reasoning_output_tokens\":0,\"total_tokens\":{total}}}}}}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"ordinal\":4,\"timestamp\":\"{at}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{turn}\"}}}}"
        )
        .unwrap();
        let mut probe = CodexAdapter::new(AdapterHome {
            home: codex_home,
            cwd: "/w".into(),
            pid: None,
            launched_at: None,
            resume_session_id: None,
        });
        probe.bind_rollout(session_id, rollout);
        let observed = probe.poll().unwrap();
        let stamp_ctx = StampCtx {
            instance_id: remuda_protocol::InstanceId::new(),
            host_id: remuda_protocol::HostId::new(),
            journal_id: remuda_protocol::Id::new("obj").unwrap(),
            run_id: remuda_protocol::RunId::new(),
            session_id: session_id.into(),
        };
        observed
            .iter()
            .enumerate()
            .find_map(|(i, obs)| match &obs.payload {
                ObservationPayload::Usage(_) => {
                    let stamped = stamp(&stamp_ctx, i as u64 + 1, obs).unwrap();
                    let rec = JournalRecord {
                        instance_id: "probe".into(),
                        seq: i as i64 + 1,
                        event_id: stamped.event_id.as_id().as_str().to_owned(),
                        event: serde_json::to_value(&stamped).unwrap(),
                        observed_at: at.to_owned(),
                    };
                    let row = project_usage_event(&rec, Some("pvp_r14"), Some("gpt-5.4")).unwrap();
                    (row.scope == "message").then_some(row)
                }
                _ => None,
            })
            .expect("the turn emits one durable Message observation")
    }

    #[test]
    fn a_failed_usage_migration_rolls_back_and_the_hub_store_still_opens() {
        // c-usagefu r14 item 1 (Ruling M1), regression (c): a failure late in
        // the migration (here the unique-index name collides with an object
        // already in the database, so CREATE UNIQUE INDEX errors AFTER the
        // backfill and collapse ran) must roll the WHOLE migration back —
        // including the ALTER TABLE-added columns — and the Hub store must
        // still open, repeatedly, on the old shape. The collision is a
        // committed pre-existing table, so it reproduces on every reopen.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hub.sqlite");
        let at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        {
            let pre = Connection::open(&db_path).unwrap();
            create_main_shaped_usage_tables(&pre);
            let row = real_codex_turn_counters(
                tempfile::tempdir().unwrap().path(),
                "r14m1c",
                "t1",
                1000,
                100,
                &at,
            );
            insert_main_turn_with_journal(&pre, "ins_r14m1c", 1, "t1", &row, &at);
            insert_main_turn_with_journal(&pre, "ins_r14m1c", 5, "t1", &row, &at);
            // Sabotage the unique-index create that runs LAST in the
            // migration batch.
            pre.execute_batch("CREATE TABLE usage_events_turn_dedupe(x TEXT NOT NULL);")
                .unwrap();
        }

        // The opener logs the failure and opens anyway — twice, since the
        // collision persists across reopens.
        let store = crate::store::Store::open(dir.path())
            .expect("Hub store opens despite the failed, rolled-back migration");
        drop(store);
        let _store2 = crate::store::Store::open(dir.path())
            .expect("a reopen opens on the legacy shape as well");
        drop(_store2);

        // Full rollback: the old shape is untouched — no scope_id column was
        // added (the ALTER TABLE rolled back with the failed transaction),
        // neither dedupe index exists, and both original rows survive.
        let check = Connection::open(&db_path).unwrap();
        let columns: i64 = check
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('usage_events')
                 WHERE name IN ('scope_id','turn_id','metric_revision')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            columns, 0,
            "the schema-changing migration fully rolled back"
        );
        let dedupe_indexes: i64 = check
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index'
                   AND name IN ('usage_events_turn_dedupe','usage_events_message_dedupe')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dedupe_indexes, 0, "no unique index was committed");
        let row_count: i64 = check
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE instance_id='ins_r14m1c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            row_count, 2,
            "neither original row was collapsed or deleted"
        );
    }

    #[test]
    fn a_rolled_back_usage_migration_degrades_reads_to_unknown_and_keeps_recording() {
        // c-usagefu r15 item 1: after a migration that fails and rolls back
        // (here the deterministic test sentinel), every usage READ must be
        // routed by the actual schema shape: the rollup is Unknown and the
        // aggregates are empty — never a SQL error against the missing
        // columns. Writes keep landing on the legacy insert path, and a later
        // successful migration recovers the data.
        let conn = Connection::open_in_memory().unwrap();
        create_main_shaped_usage_tables(&conn);
        let at = "2026-10-13T00:00:00.000Z";
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, mode, input_tokens, output_tokens,
                 accounting, observed_at)
             VALUES ('ins_r15legacy', 1, 'turn', 'snapshot', 40, 30,
                     'estimated', ?1)",
            params![at],
        )
        .unwrap();
        conn.execute_batch("CREATE TABLE remuda_test_force_usage_migration_failure (x);")
            .unwrap();

        let failed = migrate(&conn);
        assert!(
            failed.is_err(),
            "the sentinel forces a real failing statement inside the migration tx"
        );
        assert!(
            !crate::store::column_exists(&conn, "usage_events", "scope_id").unwrap(),
            "the additive DDL rolled back with the failed transaction"
        );

        // Every read degrades; no "no such column" error. c-usagefu r16 item 7:
        // the legacy shape is an explicit Unavailable state, distinct from the
        // None "no channel" state — Some(rollup) with unavailable=true, and
        // aggregates carrying unavailable=true rather than a zero that reads as
        // real spend.
        let req = RollupRequest {
            instance_id: "ins_r15legacy",
            kind: "codex",
            spec_model: Some("gpt-5.4"),
            effective_model: None,
            profile_id: Some("pvp_r15"),
        };
        let legacy_rollup = rollup_instance(&conn, &req).unwrap();
        assert!(
            legacy_rollup
                .as_ref()
                .is_some_and(|rollup| rollup.unavailable),
            "legacy shape reports an explicit Unavailable rollup, not None"
        );
        assert_eq!(
            legacy_rollup.as_ref().unwrap().context_pct,
            None,
            "an Unavailable rollup never carries a guessed percentage"
        );
        let inst_agg = aggregate_instance(&conn, "ins_r15legacy").unwrap();
        assert!(inst_agg.unavailable, "instance aggregate is Unavailable");
        assert_eq!(inst_agg.events, 0);
        let supply_agg = aggregate_supply(&conn, "pvp_r15", "gpt-5.4").unwrap();
        assert!(supply_agg.unavailable, "supply aggregate is Unavailable");
        assert_eq!(supply_agg.events, 0);

        // Writes keep recording through main's insert path (seq 2).
        let legacy_row = UsageEventRow {
            turn_id: None,
            native_context_window: None,
            instance_id: "ins_r15legacy".into(),
            seq: 2,
            profile_id: Some("pvp_r15".into()),
            model: Some("gpt-5.4".into()),
            scope: "turn".into(),
            scope_id: Some("must-not-be-persisted".into()),
            source_session_id: None,
            mode: "snapshot".into(),
            total_tokens: Some(70),
            input_tokens: Some(40),
            output_tokens: Some(30),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_usd: Some("0.07".into()),
            accounting: "estimated".into(),
            observed_at: at.into(),
            observed_at_source: "native".into(),
            metric_revision: 1,
        };
        assert!(insert_usage_event(&conn, &legacy_row).unwrap());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE instance_id='ins_r15legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "the event was recorded on the legacy shape");

        // Once the failure condition is gone the migration commits and the
        // rollup reads the recovered rows again.
        conn.execute_batch("DROP TABLE remuda_test_force_usage_migration_failure;")
            .unwrap();
        migrate(&conn).unwrap();
        assert!(crate::store::column_exists(&conn, "usage_events", "scope_id").unwrap());
        assert_eq!(
            rollup_instance(&conn, &req).unwrap().unwrap().turns,
            2,
            "the two recorded legacy NULL-scope turns fold again after recovery"
        );
    }

    fn create_usagefu_shaped_usage_tables(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE journal (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_id TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );
             CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                scope_id TEXT,
                turn_id TEXT,
                mode TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                observed_at_source TEXT NOT NULL DEFAULT 'ingest',
                native_context_window INTEGER,
                metric_revision INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (instance_id, seq)
             );",
        )
        .unwrap();
    }

    #[test]
    fn a_failed_migration_from_the_usagefu_shape_degrades_without_sql_errors() {
        // r37 item 4: the main-parent cbac58e1 schema already HAS scope_id
        // (and turn_id / metric_revision / observed_at_source /
        // native_context_window); only source_session_id and lineage_state are
        // missing. When the new migration fails against a populated such
        // database, the additive DDL rolls back, but a current-schema probe
        // that checked scope_id alone reported "current": rollups then prepared
        // SQL against the missing columns (SQL errors), and inserts skipped the
        // legacy fallback. The probe must recognise the COMPLETE schema this
        // delivery requires; reads then degrade to Unavailable/Unknown and the
        // Hub still opens.
        let conn = Connection::open_in_memory().unwrap();
        create_usagefu_shaped_usage_tables(&conn);
        let at = "2026-10-13T00:00:00.000Z";
        // A retained usagefu-era row, written through that release's full
        // column set (scope_id present).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, turn_id,
                 mode, total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                 observed_at, observed_at_source, native_context_window,
                 metric_revision)
             VALUES ('ins_r37i4', 1, 'pvp', 'm', 'turn', 'msg-1', 'nt-1',
                 'snapshot', 110, 100, 10, 0, 0, '0.01', 'estimated',
                 ?1, 'native', 200000, 1)",
            params![at],
        )
        .unwrap();

        conn.execute_batch("CREATE TABLE remuda_test_force_usage_migration_failure (x);")
            .unwrap();
        let failed = migrate(&conn);
        assert!(failed.is_err(), "the sentinel fails the new migration");
        assert!(
            crate::store::column_exists(&conn, "usage_events", "scope_id").unwrap(),
            "the usagefu column survives: this is NOT the much older shape"
        );
        assert!(
            !crate::store::column_exists(&conn, "usage_events", "source_session_id").unwrap(),
            "the new lineage column rolled back with the failed migration"
        );
        assert!(!crate::store::column_exists(&conn, "usage_events", "lineage_state").unwrap());

        assert!(
            !usage_schema_is_current(&conn).unwrap(),
            "scope_id present but the delivery-required lineage columns are \
             absent: not current, despite the failed migration"
        );

        // Every read degrades to Unavailable/Unknown — never a SQL error
        // against the missing lineage columns.
        let req = RollupRequest {
            instance_id: "ins_r37i4",
            kind: "claude",
            spec_model: Some("m"),
            effective_model: None,
            profile_id: Some("pvp"),
        };
        let rollup = rollup_instance(&conn, &req).unwrap();
        assert!(
            rollup.as_ref().is_some_and(|r| r.unavailable),
            "the near-current shape still reports an explicit Unavailable rollup"
        );
        assert!(
            aggregate_instance(&conn, "ins_r37i4").unwrap().unavailable,
            "instance aggregate is Unavailable, not a SQL error"
        );
        assert!(
            aggregate_supply(&conn, "pvp", "m").unwrap().unavailable,
            "supply aggregate is Unavailable, not a SQL error"
        );

        // Writes still land on the legacy fallback insert.
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r37i4".into(),
                    seq: 2,
                    profile_id: Some("pvp".into()),
                    model: Some("m".into()),
                    scope: "turn".into(),
                    scope_id: Some("msg-2".into()),
                    turn_id: Some("nt-2".into()),
                    native_context_window: Some(200_000),
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: None,
                    total_tokens: Some(55),
                    input_tokens: Some(50),
                    output_tokens: Some(5),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: Some("0.02".into()),
                    accounting: "estimated".into(),
                    observed_at: at.into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE instance_id='ins_r37i4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "the event was recorded through the fallback path");

        // The Hub store opens on the failed, near-current shape — a file-based
        // reopen goes through the same Store::open continue-on-error path.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hub.sqlite");
        {
            let pre = Connection::open(&db_path).unwrap();
            create_usagefu_shaped_usage_tables(&pre);
            pre.execute_batch("CREATE TABLE remuda_test_force_usage_migration_failure (x);")
                .unwrap();
        }
        let store = crate::store::Store::open(dir.path())
            .expect("Hub opens despite the failed migration from the usagefu shape");
        drop(store);

        // Once the fault clears, the migration commits and reads fold again.
        conn.execute_batch("DROP TABLE remuda_test_force_usage_migration_failure;")
            .unwrap();
        migrate(&conn).unwrap();
        assert!(crate::store::column_exists(&conn, "usage_events", "source_session_id").unwrap());
        assert!(crate::store::column_exists(&conn, "usage_events", "lineage_state").unwrap());
        assert!(usage_schema_is_current(&conn).unwrap());
        assert_eq!(
            rollup_instance(&conn, &req).unwrap().unwrap().turns,
            2,
            "both retained rows fold after recovery"
        );
    }

    #[tokio::test]
    async fn a_failed_usage_migration_leaves_every_instance_api_usable_with_usage_unknown() {
        // c-usagefu r15 item 1, the endpoint regression: with a PERSISTENT
        // migration failure (the sabotaged index name collides on every open)
        // the Hub opens and the instance list/detail/usage paths all SUCCEED —
        // the detail load runs the real rollup SQL, which must route by the
        // legacy schema shape and report usage Unknown rather than surface a
        // SQL error.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hub.sqlite");
        let at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        {
            let pre = Connection::open(&db_path).unwrap();
            create_main_shaped_usage_tables(&pre);
            let row = real_codex_turn_counters(
                tempfile::tempdir().unwrap().path(),
                "r15m1api",
                "t1",
                1000,
                100,
                &at,
            );
            insert_main_turn_with_journal(&pre, "ins_r15m1api", 1, "t1", &row, &at);
            pre.execute_batch("CREATE TABLE usage_events_turn_dedupe(x TEXT NOT NULL);")
                .unwrap();
        }

        let store = crate::store::Store::open(dir.path())
            .expect("Hub opens despite the persistently failing migration");

        // A real enrolled host and instance so list/detail load actual rows.
        let host_id: String = remuda_protocol::HostId::new().into();
        // 64 hex chars: the token-prefix index only covers that shape.
        let token = "a".repeat(64);
        store
            .insert_enroll_token(
                token.clone(),
                crate::auth::token_prefix(&token).map(str::to_string),
                "dev_test".into(),
                "2099-01-01T00:00:00.000Z".into(),
            )
            .await
            .unwrap();
        store
            .authenticate_host(
                crate::store::HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id.clone()),
                    label: Some("r15m1-node".into()),
                    node_version: Some("test".into()),
                },
                |presented, hash| presented == hash,
                |_| Ok("test-hash".into()),
            )
            .await
            .expect("host enrolls");
        let instance = store
            .insert_instance(
                host_id,
                None,
                "codex".into(),
                "shell-pty".into(),
                Some("r15m1 instance".into()),
                serde_json::json!({}),
            )
            .await
            .expect("instance insert");
        let instance_id = instance.instance_id.clone();

        // LIST: every row runs rollup_instance inside the load mapping.
        let listed = store
            .list_instances(None)
            .await
            .expect("instance list succeeds on the legacy usage shape");
        assert!(
            listed
                .iter()
                .any(|record| record.instance_id == instance_id),
            "the instance is listed"
        );
        assert!(
            listed.iter().all(|record| record
                .usage_rollup
                .as_ref()
                .is_some_and(|rollup| rollup.unavailable)),
            "every row carries an explicit Unavailable rollup on the legacy shape"
        );

        // DETAIL: same load path, single instance.
        let detail = store
            .get_instance(instance_id.clone())
            .await
            .expect("instance detail succeeds")
            .expect("the instance exists");
        assert!(
            detail
                .usage_rollup
                .as_ref()
                .is_some_and(|rollup| rollup.unavailable),
            "detail reports an explicit Unavailable rollup on the legacy shape"
        );

        // USAGE endpoint query: an Unavailable aggregate, never an error.
        let agg = store
            .run_named("r15m1_usage", move |conn| {
                aggregate_instance(conn, &instance_id).map_err(crate::store::StoreError::from)
            })
            .await
            .expect("the usage aggregate query succeeds");
        assert_eq!(agg.events, 0);
        assert!(
            agg.unavailable,
            "the zero placeholder is explicitly Unavailable, not real zero spend"
        );

        drop(store);
        // Reopen still works: the failure persists and is still non-fatal.
        let reopened = crate::store::Store::open(dir.path()).expect("reopen succeeds");
        assert!(reopened.list_instances(None).await.is_ok());
    }

    #[test]
    fn a_token_growing_point_with_a_reset_lower_cost_is_accepted_without_the_stale_cost() {
        // r4 item 4: priced turn (session stock 0.50, tokens 1000) -> an
        // UNPRICED model turn emits no cost (sticky None) -> the next PRICED
        // turn's driver total restarts at 0.02 while tokens grow. The old
        // check froze this point until the running total passed 0.50, stalling
        // the chip; it must be accepted with the reset cost dropped.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(insert(
            &conn,
            &priced_session_point(1, "s", 1, 1000, Some("0.50"))
        ));
        // Point 2: tokens grew, the priced running total restarted lower.
        let accepted = insert(&conn, &priced_session_point(2, "s", 2, 2000, Some("0.02")));
        assert!(
            accepted,
            "a cost decrease alongside token growth is a driver reset, not a freeze"
        );

        // The reset cost was NOT persisted (no sub-0.50 cumulative number).
        let stored: Option<String> = conn
            .query_row(
                "SELECT cost_usd FROM usage_events
                 WHERE instance_id='ins_test' AND seq=2 AND scope='session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, None, "the stale reset cost is dropped");

        // A later token-LESS result point with a genuine lower cumulative
        // cost is still frozen (the result channel stays monotonic).
        assert!(
            !insert(&conn, &tokenless_session_record(3, "s", 3, Some("0.01"))),
            "a token-less cost decrease is still a freeze"
        );
    }

    fn priced_session_point(
        seq: i64,
        scope_id: &str,
        revision: i64,
        input: i64,
        cost: Option<&str>,
    ) -> JournalRecord {
        let mut rec = scoped_record(
            seq,
            "ins_test",
            "session",
            Some(scope_id),
            revision,
            input,
            0,
            0,
            0,
            None,
        );
        rec.event["payload"]["cost"] = match cost {
            Some(amount) => json!({
                "state": "known",
                "value": { "amount": amount, "currency": "USD" }
            }),
            None => json!({ "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] }),
        };
        rec
    }

    #[test]
    fn message_rows_carry_native_time_for_rates_and_last_turn_at() {
        // c-usagefu r7 item 7: a real CodexAdapter Message observation carries
        // the token_usage_record's native timestamp; after stamping →
        // projection → insertion the rollup must report non-NULL 60 s / 5 min
        // rates and an advanced lastTurnAt (not ingest time).
        use remuda_driver::adapters::{
            AdapterHome, AdapterObservation, CodexAdapter, FileSignalAdapter, StampCtx, stamp,
        };
        use remuda_protocol::ObservationPayload;

        let home_dir = tempfile::tempdir().unwrap();
        let session_dir = home_dir.path().join(".codex/sessions/2026/09/15");
        std::fs::create_dir_all(&session_dir).unwrap();
        let rollout = session_dir.join("rollout-native-time.jsonl");
        let native_at = "2026-09-15T01:00:00.000Z";
        std::fs::write(
            &rollout,
            format!(
                "{{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"nt\",\"session_id\":\"nt\",\"cwd\":\"/w\",\"timestamp\":\"{native_at}\"}}}}\n\
                 {{\"ordinal\":1,\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"t1\",\"model\":\"gpt-5.4\"}}}}\n\
                 {{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"t1\"}}}}\n\
                 {{\"ordinal\":3,\"timestamp\":\"{native_at}\",\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"t1\",\"response_id\":\"r1\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":10000,\"reasoning_output_tokens\":0,\"total_tokens\":10000}}}}}}\n\
                 {{\"ordinal\":4,\"timestamp\":\"{native_at}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n"
            ),
        )
        .unwrap();

        let mut adapter = CodexAdapter::new(AdapterHome {
            home: home_dir.path().join(".codex"),
            cwd: "/w".into(),
            pid: None,
            launched_at: None,
            resume_session_id: None,
        });
        adapter.bind_rollout("nt", rollout.clone());
        let observed = adapter.poll().unwrap();
        let messages: Vec<&AdapterObservation> = observed
            .iter()
            .filter(|o| {
                matches!(&o.payload, ObservationPayload::Usage(u) if u.scope == remuda_protocol::UsageScope::Message)
            })
            .collect();
        assert!(!messages.is_empty(), "a Message usage was emitted");
        assert!(
            messages[0].native_at.is_some(),
            "Message observation carries native_at directly"
        );

        // Stamp and project.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let stamp_ctx = StampCtx {
            instance_id: remuda_protocol::InstanceId::new(),
            host_id: remuda_protocol::HostId::new(),
            journal_id: remuda_protocol::Id::new("obj").unwrap(),
            run_id: remuda_protocol::RunId::new(),
            session_id: "nt".into(),
        };
        for (i, obs) in observed.iter().enumerate() {
            let stamped = stamp(&stamp_ctx, i as u64 + 1, obs).unwrap();
            if let ObservationPayload::Usage(_) = &stamped.body {
                let event = serde_json::to_value(&stamped).unwrap();
                let rec = JournalRecord {
                    instance_id: "ins_nt".into(),
                    seq: i as i64 + 1,
                    event_id: stamped.event_id.as_id().as_str().to_owned(),
                    event,
                    observed_at: "2030-01-01T00:00:00.000Z".into(), // far future ingest
                };
                let row = project_usage_event(&rec, None, None).unwrap();
                insert_row(&conn, &row, IngestEra::Live).unwrap();
            }
        }

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_nt",
                kind: "codex",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        // Rates use NATIVE time (the row is recent relative to the native
        // timestamp only when run near 2026 — instead assert the row projected
        // with observed_at_source=native and lastTurnAt equals the native time).
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some(native_at),
            "lastTurnAt advances to the Message's native time, not ingest time"
        );
        let source: String = conn
            .query_row(
                "SELECT observed_at_source FROM usage_events WHERE scope='message' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "native", "Message row projected as native-sourced");
        // And the observed_at itself is the native timestamp.
        let at: String = conn
            .query_row(
                "SELECT observed_at FROM usage_events WHERE scope='message' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(at, native_at);
    }

    #[test]
    fn message_upsert_fills_turn_id_and_native_time_on_replay() {
        // c-usagefu r8 item 5: the FIRST write of a response lacked turn_id
        // and carried ingest time (older adapter build); a replayed row with
        // the same response id supplies them and must UPDATE the stored row,
        // not be dropped by the unique index.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // First write: no native timestamp (ingest), no turn_id.
        let first = JournalRecord {
            instance_id: "ins_upsert".into(),
            seq: 1,
            event_id: "e1".into(),
            event: json!({
                "kind":"usage",
                "payload":{
                    "scope":"message","scopeId":"r1","mode":"snapshot","metricRevision":"1",
                    "inputTokens":{"state":"known","value":"0"},
                    "outputTokens":{"state":"known","value":"100"},
                    "cost":{"state":"known","value":{"amount":"0.10","currency":"USD"}},
                    "accounting":"estimated",
                }
            }),
            observed_at: "2030-01-01T00:00:00Z".into(),
        };
        let first_row = project_usage_event(&first, None, None).unwrap();
        assert!(insert_row(&conn, &first_row, IngestEra::Live).unwrap());
        let (turn, source): (Option<String>, String) = conn
            .query_row(
                "SELECT turn_id, observed_at_source FROM usage_events WHERE scope_id='r1'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(turn, None);
        assert_eq!(source, "ingest");

        // Replay with native timestamp + turn_id (new seq).
        let native = "2026-09-15T00:00:00.000Z";
        let second = JournalRecord {
            instance_id: "ins_upsert".into(),
            seq: 2,
            event_id: "e2".into(),
            event: json!({
                "kind":"usage",
                "nativeAt":{"state":"known","value":native},
                "source":{"nativeTurnId":{"state":"known","value":"t7"}},
                "payload":{
                    "scope":"message","scopeId":"r1","mode":"snapshot","metricRevision":"1",
                    "inputTokens":{"state":"known","value":"0"},
                    "outputTokens":{"state":"known","value":"100"},
                    "cost":{"state":"known","value":{"amount":"0.10","currency":"USD"}},
                    "accounting":"estimated",
                }
            }),
            observed_at: "2030-01-01T00:00:00Z".into(),
        };
        let second_row = project_usage_event(&second, None, None).unwrap();
        let changed = insert_row(&conn, &second_row, IngestEra::Live).expect("upsert succeeds");
        assert!(changed, "the better-attributed replay updated the row");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE scope_id='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "still one row");
        let (turn, source, at): (Option<String>, String, String) = conn
            .query_row(
                "SELECT turn_id, observed_at_source, observed_at FROM usage_events WHERE scope_id='r1'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())),
            )
            .unwrap();
        assert_eq!(turn.as_deref(), Some("t7"));
        assert_eq!(source, "native");
        assert_eq!(at, native);
    }

    #[test]
    fn message_with_complete_attribution_repairs_unknown_cost_on_replay() {
        // c-usagefu r10 item 8 (Ruling C NULL->known repair): a Message is
        // ingested with COMPLETE attribution (native turn id + native time) but
        // an UNKNOWN cost (model not priced yet). A later byte-0 replay with the
        // same response id but a KNOWN cost must repair cost_usd even though no
        // attribution field is missing. The row stays singular and keeps its
        // attribution; a present value is never downgraded.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let native = "2026-09-15T00:00:00.000Z";
        let record = |seq: i64, cost: serde_json::Value| JournalRecord {
            instance_id: "ins_repair".into(),
            seq,
            event_id: format!("e{seq}"),
            event: json!({
                "kind": "usage",
                "nativeAt": {"state": "known", "value": native},
                "source": {"nativeTurnId": {"state": "known", "value": "t8"}},
                "payload": {
                    "scope": "message", "scopeId": "r8", "mode": "snapshot",
                    "metricRevision": "1",
                    "inputTokens": {"state": "known", "value": "0"},
                    "outputTokens": {"state": "known", "value": "100"},
                    "cost": cost,
                    "accounting": "estimated"
                }
            }),
            observed_at: native.into(),
        };
        // First ingest: complete attribution, unknown cost.
        let first = record(
            1,
            json!({"state": "unknown", "reason": "unpriced-model", "evidenceEventIds": []}),
        );
        let first_row = project_usage_event(&first, Some("pvp_repair"), Some("gpt-5.4")).unwrap();
        assert!(insert_usage_event(&conn, &first_row).unwrap());
        let (turn, source, cost): (Option<String>, String, Option<String>) = conn
            .query_row(
                "SELECT turn_id, observed_at_source, cost_usd FROM usage_events WHERE scope_id='r8'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())),
            )
            .unwrap();
        assert_eq!(turn.as_deref(), Some("t8"), "attribution already complete");
        assert_eq!(source, "native");
        assert_eq!(cost, None, "cost unknown on first ingest");

        // Replay (new seq, same durable response id) now carries a known cost.
        let replay = record(
            2,
            json!({"state": "known", "value": {"amount": "0.10", "currency": "USD"}}),
        );
        let replay_row = project_usage_event(&replay, Some("pvp_repair"), Some("gpt-5.4")).unwrap();
        let changed = insert_usage_event(&conn, &replay_row).unwrap();
        assert!(changed, "the NULL->known cost repair updates the row");

        let (turn, source, at, cost): (Option<String>, String, String, Option<String>) = conn
            .query_row(
                "SELECT turn_id, observed_at_source, observed_at, cost_usd
                 FROM usage_events WHERE scope_id='r8'",
                [],
                |r| {
                    Ok((
                        r.get(0).unwrap(),
                        r.get(1).unwrap(),
                        r.get(2).unwrap(),
                        r.get(3).unwrap(),
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            turn.as_deref(),
            Some("t8"),
            "attribution unchanged by repair"
        );
        assert_eq!(source, "native");
        assert_eq!(at, native);
        assert_eq!(
            cost.as_deref(),
            Some("0.10"),
            "known cost repaired on replay"
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE scope_id='r8'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "the replay repairs the one durable row");

        // The repair flows through to billing.
        let agg = aggregate_instance(&conn, "ins_repair").unwrap();
        assert!(
            (agg.cost_usd.unwrap() - 0.10).abs() < 1e-9,
            "repaired cost is billable"
        );
    }

    #[test]
    fn duplicate_message_updates_are_revision_authoritative_for_accounting() {
        // c-usagefu r11 item 9 (Ruling C1, EXACT) through the public ingest
        // path: every record is projected from a real usage observation into
        // the durable Message row r9.
        //   seq 1, rev 1: output 10 / cost 0.01 / estimated
        //   seq 2, rev 2: output 20 / cost 0.02 / reported   -> REPLACES, rev 2
        //   seq 3, rev 1: output 99 / cost 0.99 / estimated  -> OLDER, inert
        //   seq 4, rev 2: output unknown, cache_read 50, cost unknown
        //                 -> same-revision NULL->known repair of cache only
        //   seq 5, rev 3: output 25 / cost 0.03 / reported  -> REPLACES, rev 3
        // An older revision never downgrades counters, cost or accounting; a
        // same/newer revision fills a NULL bucket and replaces present
        // accounting outright.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let native = "2026-09-15T00:00:00.000Z";
        let known = |v: &str| json!({"state": "known", "value": v});
        let unknown =
            json!({"state": "unknown", "reason": "unpriced-model", "evidenceEventIds": []});
        let record = |seq: i64,
                      rev: u32,
                      output: serde_json::Value,
                      cache_read: Option<u64>,
                      cost: serde_json::Value,
                      accounting: &str| JournalRecord {
            instance_id: "ins_c1live".into(),
            seq,
            event_id: format!("e{seq}"),
            event: json!({
                "kind": "usage",
                "nativeAt": {"state": "known", "value": native},
                "source": {"nativeTurnId": {"state": "known", "value": "t9"}},
                "payload": {
                    "scope": "message", "scopeId": "r9", "mode": "snapshot",
                    "metricRevision": rev.to_string(),
                    "inputTokens": known("0"),
                    "outputTokens": output,
                    "cacheReadTokens": cache_read
                        .map_or(json!({"state":"unknown"}), |c| known(&c.to_string())),
                    "cost": cost,
                    "accounting": accounting
                }
            }),
            observed_at: native.into(),
        };
        let ingest = |rec: &JournalRecord| {
            let row = project_usage_event(rec, Some("pvp_c1"), Some("gpt-5.4")).unwrap();
            insert_usage_event(&conn, &row).unwrap()
        };
        let state = |conn: &Connection| -> (i64, Option<i64>, Option<i64>, Option<String>, String) {
            conn.query_row(
                "SELECT metric_revision, output_tokens, cache_read_tokens, cost_usd,
                            accounting
                     FROM usage_events WHERE scope_id='r9'",
                [],
                |r| {
                    Ok((
                        r.get(0).unwrap(),
                        r.get(1).unwrap(),
                        r.get(2).unwrap(),
                        r.get(3).unwrap(),
                        r.get(4).unwrap(),
                    ))
                },
            )
            .unwrap()
        };

        assert!(ingest(&record(
            1,
            1,
            known("10"),
            None,
            json!({"state":"known","value":{"amount":"0.01","currency":"USD"}}),
            "estimated"
        )));
        assert_eq!(
            state(&conn),
            (1, Some(10), None, Some("0.01".into()), "estimated".into())
        );

        // Newer revision replaces the authoritative accounting.
        assert!(ingest(&record(
            2,
            2,
            known("20"),
            None,
            json!({"state":"known","value":{"amount":"0.02","currency":"USD"}}),
            "reported"
        )));
        assert_eq!(
            state(&conn),
            (2, Some(20), None, Some("0.02".into()), "reported".into())
        );

        // Older revision is entirely inert — no downgrade, no changed row.
        assert!(
            !ingest(&record(
                3,
                1,
                known("99"),
                Some(7),
                json!({"state":"known","value":{"amount":"0.99","currency":"USD"}}),
                "estimated"
            )),
            "the older duplicate changes nothing"
        );
        assert_eq!(
            state(&conn),
            (2, Some(20), None, Some("0.02".into()), "reported".into()),
            "an older revision never replaces even a NULL bucket"
        );

        // Same revision fills a NULL bucket, imposes nothing via its unknowns.
        assert!(ingest(&record(
            4,
            2,
            json!({"state":"unknown"}),
            Some(50),
            unknown.clone(),
            "reported"
        )));
        assert_eq!(
            state(&conn),
            (
                2,
                Some(20),
                Some(50),
                Some("0.02".into()),
                "reported".into()
            ),
            "same-revision NULL->known repair fills cache_read only"
        );

        // Newer revision replaces present values again.
        assert!(ingest(&record(
            5,
            3,
            known("25"),
            Some(60),
            json!({"state":"known","value":{"amount":"0.03","currency":"USD"}}),
            "reported"
        )));
        assert_eq!(
            state(&conn),
            (
                3,
                Some(25),
                Some(60),
                Some("0.03".into()),
                "reported".into()
            )
        );

        // One durable row all along.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE scope_id='r9'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn duplicate_message_replay_does_not_double_totals() {
        // c-usagefu r7 item 5: re-promoting Codex replays the rollout from
        // byte 0 and two fresh adapters emit the SAME Message response ids.
        // The durable unique index (instance, scope, scope_id) for Message
        // rows must collapse the replay; totals stay at one set.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let message_row = |seq: i64, response_id: &str, output: i64| {
            let rec = JournalRecord {
                instance_id: "ins_replay".into(),
                seq,
                event_id: format!("evt_replay_{seq}"),
                event: json!({
                    "kind": "usage",
                    "source": { "nativeTurnId": { "state": "known", "value": "t1" } },
                    "payload": {
                        "scope": "message",
                        "scopeId": response_id,
                        "mode": "snapshot",
                        "metricRevision": "1",
                        "inputTokens": { "state": "known", "value": "0" },
                        "outputTokens": { "state": "known", "value": output.to_string() },
                        "cost": { "state": "known",
                                   "value": { "amount": "0.10", "currency": "USD" } },
                        "accounting": "estimated",
                    }
                }),
                observed_at: "2026-09-15T00:00:00.000Z".into(),
            };
            project_usage_event(&rec, None, None).unwrap()
        };

        // First adapter: two message responses (r1=100 out, r2=200 out).
        let first = insert_row(&conn, &message_row(1, "r1", 100), IngestEra::Live).unwrap();
        assert!(first, "first r1 inserted");
        assert!(insert_row(&conn, &message_row(2, "r2", 200), IngestEra::Live).unwrap());

        // Second adapter replays the same rollout (different journal seqs but
        // identical response ids): both must be ignored.
        assert!(
            !insert_row(&conn, &message_row(3, "r1", 100), IngestEra::Live).unwrap(),
            "replayed r1 is deduped by response id"
        );
        assert!(
            !insert_row(&conn, &message_row(4, "r2", 200), IngestEra::Live).unwrap(),
            "replayed r2 is deduped by response id"
        );

        let agg = aggregate_instance(&conn, "ins_replay").unwrap();
        assert_eq!(agg.output_tokens, Some(300), "100 + 200, not doubled");
        assert!(
            (agg.cost_usd.unwrap() - 0.20).abs() < 1e-9,
            "two responses at 0.10, replay not billed: {}",
            agg.cost_usd.unwrap()
        );
    }

    #[test]
    fn message_and_session_cumulative_history_are_never_added() {
        // c-usagefu r8 item 4: Message per-call rows AND cumulative Session
        // snapshots describe the SAME calls, in the REAL codex observation
        // shape — each turn emits one additive Message and one cumulative
        // Session growth point.
        //   Message 10 + Session 10 + Message 20 + Session 30 -> 30, not 70.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let t1 = ago(4);
        let t2 = ago(2);
        // One projection for either scope: flow rows carry per-call counters,
        // session rows carry the same counters CUMULATIVELY.
        let project = |seq: i64,
                       scope: &str,
                       scope_id: &str,
                       rev: i64,
                       input: i64,
                       out: i64,
                       cache_read: i64,
                       cache_write: i64,
                       cost: &str,
                       at: &str,
                       turn: Option<&str>| {
            let mut event = json!({
                "kind":"usage",
                "nativeAt":{"state":"known","value":at},
                "payload":{
                    "scope":scope,"scopeId":scope_id,"mode":"snapshot",
                    "metricRevision":rev.to_string(),
                    "inputTokens":{"state":"known","value":input.to_string()},
                    "outputTokens":{"state":"known","value":out.to_string()},
                    "cacheReadTokens":{"state":"known","value":cache_read.to_string()},
                    "cacheWriteTokens":{"state":"known","value":cache_write.to_string()},
                    "cost":{"state":"known","value":{"amount":cost,"currency":"USD"}},
                    "accounting":"estimated",
                }
            });
            if let Some(id) = turn {
                event["source"] = json!({"nativeTurnId":{"state":"known","value":id}});
            }
            let rec = JournalRecord {
                instance_id: "ins_mix".into(),
                seq,
                event_id: format!("e{seq}"),
                event,
                observed_at: at.into(),
            };
            project_usage_event(&rec, Some("pvp_mix"), Some("gpt-5.4")).unwrap()
        };
        // Turn 1: additive Message (40/10, cache 100/5) and the cumulative
        // Session point at the same totals (cost 0.10).
        insert_row(
            &conn,
            &project(
                1,
                "message",
                "r1",
                1,
                40,
                10,
                100,
                5,
                "0.10",
                &t1,
                Some("t1"),
            ),
            IngestEra::Live,
        )
        .unwrap();
        insert_row(
            &conn,
            &project(2, "session", "s1", 1, 40, 10, 100, 5, "0.10", &t1, None),
            IngestEra::Live,
        )
        .unwrap();
        // Turn 2: additive Message (60/20, cache 150/7) and the Session point
        // now reporting the CUMULATIVE 100/30 (cache 250/12, cost 0.30).
        insert_row(
            &conn,
            &project(
                3,
                "message",
                "r2",
                1,
                60,
                20,
                150,
                7,
                "0.20",
                &t2,
                Some("t2"),
            ),
            IngestEra::Live,
        )
        .unwrap();
        insert_row(
            &conn,
            &project(4, "session", "s1", 2, 100, 30, 250, 12, "0.30", &t2, None),
            IngestEra::Live,
        )
        .unwrap();

        // Both aggregates: one representation, never flow + cumulative stock.
        let agg = aggregate_instance(&conn, "ins_mix").unwrap();
        assert_eq!(
            agg.input_tokens,
            Some(100),
            "40+60 Message, not +140 Session"
        );
        assert_eq!(
            agg.output_tokens,
            Some(30),
            "10+20 Message, not 10+10+20+30"
        );
        assert!(
            (agg.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "cumulative cost 0.30"
        );
        let supply = aggregate_supply(&conn, "pvp_mix", "gpt-5.4").unwrap();
        assert_eq!(supply.input_tokens, Some(100));
        assert_eq!(supply.output_tokens, Some(30));
        assert!((supply.cost_usd.unwrap() - 0.30).abs() < 1e-9);

        // Rollup: one flow representation, session stock used only as the
        // fallback (here flow covers it). Totals, cache and rates agree.
        let rollup = rollup_fold(&conn, "ins_mix", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(100));
        assert_eq!(rollup.session_output_tokens, Some(30));
        assert_eq!(rollup.cache_read_tokens, Some(250), "100+150, not +100+250");
        assert_eq!(rollup.cache_creation_tokens, Some(12), "5+7, not +5+12");
        assert_eq!(rollup.tpm_in_60s, Some(100), "flow rate window");
        assert_eq!(rollup.tpm_out_60s, Some(30));
        assert_eq!(rollup.tpm_out_5m, Some(6), "30 tokens / 5 minutes");
    }

    #[test]
    fn upgraded_dual_message_and_turn_rows_count_once() {
        // c-usagefu r8 item 4: an upgraded pre-r6 DB has BOTH a Message and a
        // Turn row (100 output each) per call. The rollup, instance
        // aggregate, and supply aggregate must all read 100, not 200.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let at = ago(2); // recent enough for the 60 s / 5 min rate windows
        let at = at.as_str();
        let project = |seq: i64, scope: &str, scope_id: &str| {
            let rec = JournalRecord {
                instance_id: "ins_up".into(),
                seq,
                event_id: format!("e{seq}"),
                event: json!({
                    "kind":"usage",
                    "nativeAt":{"state":"known","value":at},
                    "source":{"nativeTurnId":{"state":"known","value":"t1"}},
                    "payload":{
                        "scope":scope,"scopeId":scope_id,"mode":"snapshot","metricRevision":"1",
                        "inputTokens":{"state":"known","value":"0"},
                        "outputTokens":{"state":"known","value":"100"},
                        "cost":{"state":"known","value":{"amount":"0.10","currency":"USD"}},
                        "accounting":"estimated",
                    }
                }),
                observed_at: at.into(),
            };
            project_usage_event(&rec, Some("pvp_up"), Some("gpt-5.4")).unwrap()
        };
        insert_row(&conn, &project(1, "message", "r1"), IngestEra::Live).unwrap();
        insert_row(&conn, &project(2, "turn", "t1"), IngestEra::Live).unwrap();

        assert_eq!(
            aggregate_instance(&conn, "ins_up").unwrap().output_tokens,
            Some(100)
        );
        assert_eq!(
            aggregate_supply(&conn, "pvp_up", "gpt-5.4")
                .unwrap()
                .output_tokens,
            Some(100)
        );
        let rollup = rollup_fold(&conn, "ins_up", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(100));
        assert_eq!(
            rollup.tpm_out_60s,
            Some(100),
            "rate window also reads one stream"
        );
    }

    #[test]
    fn pre_r6_dual_scope_rows_bill_once_from_message_stream() {
        // c-usagefu r7 item 4: databases written BEFORE r6 hold BOTH a Message
        // and a Turn row per priced codex call (the old adapter emitted both).
        // Token sums must not double and the priced/unpriced/priced cost must
        // be 0.30 (Message flow), not 0.60 (Message + Turn) and not frozen.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let project =
            |seq: i64, scope: &str, scope_id: &str, turn_id: Option<&str>, cost: Option<&str>| {
                let mut event = json!({
                    "kind": "usage",
                    "source": { "nativeTurnId": turn_id.map(|t| json!({
                        "state": "known", "value": t })) },
                    "payload": {
                        "scope": scope,
                        "scopeId": scope_id,
                        "mode": "snapshot",
                        "metricRevision": "1",
                        "inputTokens": { "state": "known", "value": "0" },
                        "outputTokens": { "state": "known", "value": "10000" },
                        "cost": if let Some(amount) = cost {
                            json!({ "state": "known",
                                     "value": { "amount": amount, "currency": "USD" } })
                        } else {
                            json!({ "state": "unknown", "reason": "unpriced-model-or-no-usage",
                                     "evidenceEventIds": [] })
                        },
                        "accounting": "estimated",
                        "nativeAt": { "state": "known",
                                       "value": "2026-09-15T00:00:00.000Z" },
                    }
                });
                if turn_id.is_none() {
                    event["source"]
                        .as_object_mut()
                        .unwrap()
                        .remove("nativeTurnId");
                }
                let rec = JournalRecord {
                    instance_id: "ins_dual".into(),
                    seq,
                    event_id: format!("evt_dual_{seq}"),
                    event,
                    observed_at: "2026-09-15T00:00:00.000Z".into(),
                };
                project_usage_event(&rec, Some("pvp_dual"), Some("gpt-5.4")).unwrap()
            };
        let s1 = |seq: i64, revision: i64, cost: Option<&str>| {
            let rec = tokenless_session_record_for("ins_dual", seq, "s1", revision, cost);
            project_usage_event(&rec, Some("pvp_dual"), Some("gpt-5.4")).unwrap()
        };
        // Turn 1: Message r1 ($0.10) AND Turn t1 ($0.10).
        let mut seq = 1i64;
        insert_row(
            &conn,
            &project(seq, "message", "r1", Some("t1"), Some("0.10")),
            IngestEra::Live,
        )
        .unwrap();
        seq += 1;
        insert_row(
            &conn,
            &project(seq, "turn", "t1", Some("t1"), Some("0.10")),
            IngestEra::Live,
        )
        .unwrap();
        seq += 1;
        // Session cumulative point priced $0.10.
        insert_usage_event(&conn, &s1(seq, 1, Some("0.10"))).unwrap();
        seq += 1;
        // Turn 2 unpriced: Message + Turn both unknown cost; session NULL.
        insert_row(
            &conn,
            &project(seq, "message", "r2", Some("t2"), None),
            IngestEra::Live,
        )
        .unwrap();
        seq += 1;
        insert_row(
            &conn,
            &project(seq, "turn", "t2", Some("t2"), None),
            IngestEra::Live,
        )
        .unwrap();
        seq += 1;
        insert_usage_event(&conn, &s1(seq, 2, None)).unwrap();
        seq += 1;
        // Turn 3 priced $0.20: Message + Turn + session.
        insert_row(
            &conn,
            &project(seq, "message", "r3", Some("t3"), Some("0.20")),
            IngestEra::Live,
        )
        .unwrap();
        seq += 1;
        insert_row(
            &conn,
            &project(seq, "turn", "t3", Some("t3"), Some("0.20")),
            IngestEra::Live,
        )
        .unwrap();
        seq += 1;
        insert_usage_event(&conn, &s1(seq, 3, None)).unwrap();

        let instance = aggregate_instance(&conn, "ins_dual").unwrap();
        assert!(
            (instance.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "dual-scope rows bill once (Message stream): 0.30, got {}",
            instance.cost_usd.unwrap()
        );
        // Output tokens: 3 Message rows x 10000 = 30000, NOT 60000.
        assert_eq!(
            instance.output_tokens,
            Some(30_000),
            "tokens are not doubled across Message+Turn"
        );
        let supply = aggregate_supply(&conn, "pvp_dual", "gpt-5.4").unwrap();
        assert!(
            (supply.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "supply also unduplicated, got {}",
            supply.cost_usd.unwrap()
        );
        assert_eq!(supply.output_tokens, Some(30_000));
    }

    #[test]
    fn a_null_newest_session_point_switches_the_instance_to_flow_billing() {
        // r5 item 3 exact regression sequence (r4 sticky-Unknown driver):
        //   turn 1: priced session stock 0.50 (turn row 0.10)
        //   turn 2: unpriced model -> session point NULL (turn row unpriced)
        //   turn 3: priced again -> session point STILL NULL (sticky Unknown),
        //            turn row 0.20
        // r4 ranked only priced session rows, so the stock stayed 0.50 and
        // the instance was excluded from flow: /usage froze and the stop band
        // was unreachable. Now a NULL newest point makes the instance
        // stock-LESS and the priced per-call flow (0.10 + 0.20) is the bill.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let project = |rec: &JournalRecord| {
            project_usage_event(rec, Some("pvp_sticky"), Some("gw/sticky")).unwrap()
        };
        // Turn 1: priced stock + priced turn estimate.
        assert!(
            insert_usage_event(
                &conn,
                &project(&priced_turn_record("ins_sticky", 1, "msg_1", "0.10"))
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &project(&tokenless_session_record_for(
                    "ins_sticky",
                    2,
                    "s1",
                    1,
                    Some("0.50")
                ))
            )
            .unwrap()
        );
        assert!(
            (aggregate_instance(&conn, "ins_sticky")
                .unwrap()
                .cost_usd
                .unwrap()
                - 0.10)
                .abs()
                < 1e-9,
            "eligible flow exists: the priced Turn 0.10 is the bill, stock ignored (r10 item 6)"
        );
        // Turn 2: unpriced session point becomes the newest point.
        assert!(
            insert_usage_event(
                &conn,
                &project(&tokenless_session_record_for(
                    "ins_sticky",
                    3,
                    "s1",
                    2,
                    None
                ))
            )
            .unwrap()
        );
        // Turn 3: session still NULL-cost, but a priced per-call row arrives.
        assert!(
            insert_usage_event(
                &conn,
                &project(&priced_turn_record("ins_sticky", 4, "msg_3", "0.20"))
            )
            .unwrap()
        );
        // (The NULL point at seq 3 is already the newest session point; a
        // second identical NULL point is frozen as a duplicate.)

        let agg = aggregate_instance(&conn, "ins_sticky").unwrap();
        assert!(
            (agg.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "sticky-NULL newest stock -> flow 0.10 + 0.20, got {}",
            agg.cost_usd.unwrap()
        );
        // The supply aggregate follows the same instance-level rule.
        let supply = aggregate_supply(&conn, "pvp_sticky", "gw/sticky").unwrap();
        assert!(
            (supply.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "supply advances on flow after the NULL newest point, got {}",
            supply.cost_usd.unwrap()
        );
    }

    #[test]
    fn legacy_null_scope_cost_is_ignored_once_an_instance_has_scoped_sessions() {
        // r5 item 7: a session that straddled the scopeId upgrade has both
        // legacy NULL-scope rows and newer scoped rows. The legacy chain and
        // the scoped partitions are the SAME cumulative session, so summing
        // both double-counts. Scoped wins (matching the token rule): once any
        // scoped row exists, NULL-scope stock contributes nothing.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Legacy world: NULL scope_id priced at 0.40.
        let mut legacy = tokenless_session_record(1, "ignored-scope", 1, Some("0.40"));
        legacy.event["payload"]
            .as_object_mut()
            .unwrap()
            .remove("scopeId");
        assert!(insert(&conn, &legacy));
        // Post-upgrade: scoped cumulative 0.50.
        assert!(insert(
            &conn,
            &tokenless_session_record_for("ins_test", 2, "s1", 1, Some("0.50"))
        ));

        let agg = aggregate_instance(&conn, "ins_test").unwrap();
        assert!(
            (agg.cost_usd.unwrap() - 0.50).abs() < 1e-9,
            "scoped stock alone counts (no 0.40 + 0.50), got {}",
            agg.cost_usd.unwrap()
        );

        // With no scoped rows the legacy NULL-scope chain still works on its
        // own.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut legacy = tokenless_session_record(1, "ignored-scope", 1, Some("0.30"));
        legacy.event["payload"]
            .as_object_mut()
            .unwrap()
            .remove("scopeId");
        assert!(insert(&conn, &legacy));
        let agg = aggregate_instance(&conn, "ins_test").unwrap();
        assert!(
            (agg.cost_usd.unwrap() - 0.30).abs() < 1e-9,
            "legacy chain still sums"
        );
    }

    fn tokenless_session_record(
        seq: i64,
        scope_id: &str,
        revision: i64,
        cost: Option<&str>,
    ) -> JournalRecord {
        tokenless_session_record_accounting(seq, scope_id, revision, cost, "estimated")
    }

    fn tokenless_session_record_accounting(
        seq: i64,
        scope_id: &str,
        revision: i64,
        cost: Option<&str>,
        accounting: &str,
    ) -> JournalRecord {
        let mut payload = serde_json::Map::new();
        payload.insert("scope".into(), json!("session"));
        payload.insert("scopeId".into(), json!(scope_id));
        payload.insert("mode".into(), json!("snapshot"));
        payload.insert("metricRevision".into(), json!(revision.to_string()));
        payload.insert(
            "cost".into(),
            cost.map_or(
                json!({ "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] }),
                |amount| {
                    json!({
                        "state": "known",
                        "value": { "amount": amount, "currency": "USD" }
                    })
                },
            ),
        );
        payload.insert("accounting".into(), json!(accounting));
        JournalRecord {
            instance_id: "ins_test".into(),
            seq,
            event_id: format!("evt_{seq}"),
            event: json!({ "kind": "usage", "payload": Value::Object(payload) }),
            observed_at: "2026-09-15T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn cost_is_the_sum_of_the_newest_stock_per_session_scope_in_one_instance() {
        // r4 item 1: a single global newest-row stock let one session answer
        // for the instance. Two native sessions (codex rebind) each carry a
        // cumulative reported cost; the bill is the newest accepted point per
        // (instance, scope_id), summed — 0.50 + 0.05, never the interleave of
        // whichever row landed last. r10 item 6 keeps this ONLY for a
        // stock-ONLY instance: no per-call eligible flow is present here, so
        // the cumulative stock is the bill (Ruling B).
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Session s1 reports 0.40 then 0.50 (only the newest counts).
        assert!(insert(
            &conn,
            &tokenless_session_record_for("ins_two", 1, "s1", 1, Some("0.40"))
        ));
        assert!(insert(
            &conn,
            &tokenless_session_record_for("ins_two", 2, "s2", 1, Some("0.05"))
        ));
        assert!(insert(
            &conn,
            &tokenless_session_record_for("ins_two", 3, "s1", 2, Some("0.50"))
        ));

        let agg = aggregate_instance(&conn, "ins_two").unwrap();
        assert!(
            (agg.cost_usd.unwrap() - 0.55).abs() < 1e-9,
            "newest per-session stocks summed (0.50 + 0.05), got {}",
            agg.cost_usd.unwrap()
        );
    }

    #[test]
    fn supply_cost_sums_session_stocks_across_instances() {
        // Ten (here two) SDK instances, each reporting a $5 cumulative session
        // stock on one profile: the profile usage is $10, not $5.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let project = |rec: &JournalRecord| {
            project_usage_event(rec, Some("pvp_cost"), Some("gw/model-cost")).unwrap()
        };
        let r1 = project(&tokenless_session_record_for(
            "ins_a",
            1,
            "s1",
            1,
            Some("5.00"),
        ));
        let r2 = project(&tokenless_session_record_for(
            "ins_b",
            1,
            "s1",
            1,
            Some("5.00"),
        ));
        assert!(insert_usage_event(&conn, &r1).unwrap());
        assert!(insert_usage_event(&conn, &r2).unwrap());

        let supply = aggregate_supply(&conn, "pvp_cost", "gw/model-cost").unwrap();
        assert!(
            (supply.cost_usd.unwrap() - 10.0).abs() < 1e-9,
            "stocks summed across instances, got {}",
            supply.cost_usd.unwrap()
        );
    }

    #[test]
    fn a_per_call_only_instance_keeps_its_flow_sum_beside_a_priced_instance() {
        // The flow fallback is keyed per INSTANCE (turn rows carry no session
        // key): instance B reports only per-call estimates and must not be
        // wiped because A carries stock. r10 item 6 (Ruling B): COST follows
        // the eligible-flow decision — A has a priced stock AND its own
        // eligible per-call Turn, so A bills that FLOW (0.10) and its stock is
        // ignored; stock-less B contributes its flow 0.02 + 0.01. Total 0.13.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let project = |rec: &JournalRecord| {
            project_usage_event(rec, Some("pvp_mix"), Some("gw/model-mix")).unwrap()
        };
        assert!(
            insert_usage_event(
                &conn,
                &project(&tokenless_session_record_for(
                    "ins_a",
                    1,
                    "s1",
                    1,
                    Some("0.50")
                ))
            )
            .unwrap()
        );
        // ins_a has eligible flow: its own turn estimate is the bill, not the
        // 0.50 stock (Ruling B item 6).
        assert!(
            insert_usage_event(
                &conn,
                &project(&priced_turn_record("ins_a", 2, "ma", "0.10"))
            )
            .unwrap()
        );
        // ins_b, stock-less, contributes its per-call flow.
        assert!(
            insert_usage_event(
                &conn,
                &project(&priced_turn_record("ins_b", 1, "mb1", "0.02"))
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &project(&priced_turn_record("ins_b", 2, "mb2", "0.01"))
            )
            .unwrap()
        );

        let supply = aggregate_supply(&conn, "pvp_mix", "gw/model-mix").unwrap();
        assert!(
            (supply.cost_usd.unwrap() - 0.13).abs() < 1e-9,
            "A's flow 0.10 + B's flow 0.03 (A stock ignored), got {}",
            supply.cost_usd.unwrap()
        );
    }

    #[test]
    fn a_reported_bill_in_one_instance_keeps_another_instances_estimate() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let project =
            |rec: &JournalRecord| project_usage_event(rec, Some("pvp_aa2"), Some("m-aa2")).unwrap();
        // A: native reported cumulative session bill 0.01.
        assert!(
            insert_usage_event(
                &conn,
                &project(&tokenless_session_record_for_accounting(
                    "ins_a_rep",
                    1,
                    "sess-a",
                    1,
                    Some("0.01"),
                    "reported"
                ))
            )
            .unwrap()
        );
        // B: a priced per-call turn, no reported bill yet.
        assert!(
            insert_usage_event(
                &conn,
                &project(&priced_turn_record("ins_b_est", 1, "mb", "0.02"))
            )
            .unwrap()
        );

        let supply = aggregate_supply(&conn, "pvp_aa2", "m-aa2").unwrap();
        assert!(
            (supply.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "A's reported 0.01 plus B's estimated 0.02, got {:?}",
            supply.cost_usd
        );
        assert_eq!(
            supply.budget_status(Some(0.02)),
            BudgetStatus::Stop,
            "the 0.03 spend must Stop a 0.02 budget"
        );
        // Per instance: each contributes only its own authority.
        assert!(
            (aggregate_instance(&conn, "ins_a_rep")
                .unwrap()
                .cost_usd
                .unwrap()
                - 0.01)
                .abs()
                < 1e-9
        );
        assert!(
            (aggregate_instance(&conn, "ins_b_est")
                .unwrap()
                .cost_usd
                .unwrap()
                - 0.02)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn a_reported_legacy_null_bill_is_suppressed_once_scoped_evidence_exists() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Legacy world: NULL scope_id, REPORTED cumulative 0.40.
        let mut legacy =
            tokenless_session_record_accounting(1, "ignored", 1, Some("0.40"), "reported");
        legacy.event["payload"]
            .as_object_mut()
            .unwrap()
            .remove("scopeId");
        legacy.instance_id = "ins_aa3".into();
        assert!(insert(&conn, &legacy));
        // Scoped REPORTED cumulative 0.50 for the ongoing session.
        assert!(
            insert_usage_event(
                &conn,
                &project_usage_event(
                    &tokenless_session_record_for_accounting(
                        "ins_aa3",
                        2,
                        "s1",
                        2,
                        Some("0.50"),
                        "reported"
                    ),
                    Some("pvp_aa3"),
                    Some("m-aa3")
                )
                .unwrap()
            )
            .unwrap()
        );

        let agg = aggregate_instance(&conn, "ins_aa3").unwrap();
        assert!(
            (agg.cost_usd.unwrap() - 0.50).abs() < 1e-9,
            "scoped reported bill alone counts, not 0.40+0.50, got {:?}",
            agg.cost_usd
        );
        let supply = aggregate_supply(&conn, "pvp_aa3", "m-aa3").unwrap();
        assert!((supply.cost_usd.unwrap() - 0.50).abs() < 1e-9);

        // A legacy-only instance (no scoped evidence anywhere) still bills its
        // NULL-scope reported cumulative value.
        let legacy_only =
            tokenless_session_record_accounting(1, "ignored", 1, Some("0.07"), "reported");
        let mut legacy_only = legacy_only;
        legacy_only.event["payload"]
            .as_object_mut()
            .unwrap()
            .remove("scopeId");
        legacy_only.instance_id = "ins_aa3_legacy".into();
        assert!(insert(&conn, &legacy_only));
        assert!(
            (aggregate_instance(&conn, "ins_aa3_legacy")
                .unwrap()
                .cost_usd
                .unwrap()
                - 0.07)
                .abs()
                < 1e-9,
            "without scoped evidence the NULL reported bill still applies"
        );
    }

    fn r40_session_row(
        instance: &str,
        seq: i64,
        sid: Option<&str>,
        revision: i64,
        input: Option<i64>,
        cost: Option<&str>,
        accounting: &str,
    ) -> UsageEventRow {
        UsageEventRow {
            instance_id: instance.into(),
            seq,
            profile_id: Some("pvp_r40".into()),
            model: Some("m-r40".into()),
            scope: "session".into(),
            scope_id: sid.map(str::to_string),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: revision,
            source_session_id: sid.map(str::to_string),
            total_tokens: input,
            input_tokens: input,
            output_tokens: input.map(|_| 0),
            cache_read_tokens: input.map(|_| 0),
            cache_write_tokens: input.map(|_| 0),
            cost_usd: cost.map(str::to_string),
            accounting: accounting.into(),
            observed_at: "2026-10-15T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        }
    }

    #[test]
    fn one_sessions_reported_bill_keeps_another_sessions_estimated_contribution() {
        let build = |s2_cost: Option<&str>| {
            let conn = Connection::open_in_memory().unwrap();
            migrate(&conn).unwrap();
            // S1: tokenless REPORTED cumulative bill.
            assert!(
                insert_usage_event(
                    &conn,
                    &r40_session_row(
                        "ins_r40i1",
                        1,
                        Some("s1"),
                        1,
                        None,
                        Some("0.01"),
                        "reported"
                    )
                )
                .unwrap()
            );
            // S2: a DIFFERENT estimated session, token stock priced 0.02.
            assert!(
                insert_usage_event(
                    &conn,
                    &r40_session_row(
                        "ins_r40i1",
                        2,
                        Some("s2"),
                        1,
                        Some(200),
                        s2_cost,
                        "estimated"
                    )
                )
                .unwrap()
            );
            conn
        };

        let conn = build(Some("0.02"));
        let inst = aggregate_instance(&conn, "ins_r40i1").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert!(
            (inst.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "S1 reported 0.01 + S2 estimated 0.02, got {:?}",
            inst.cost_usd
        );
        assert_eq!(
            inst.cost_usd, supply.cost_usd,
            "instance and supply folds agree on the same retained data"
        );
        assert_eq!(inst.total_tokens, supply.total_tokens);
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(
            inst.budget_status(Some(0.02)),
            BudgetStatus::Stop,
            "the 0.03 spend Stops a 0.02 budget"
        );

        // S2's price Unknown: the whole aggregate is Unknown.
        let conn = build(None);
        let inst = aggregate_instance(&conn, "ins_r40i1").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert_eq!(
            inst.cost_usd, None,
            "an Unknown uncovered session poisons the aggregate"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd);
        assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Unknown);
    }

    #[test]
    fn reported_authority_splits_qualified_and_unqualified_flow_estimates() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // S1: tokenless reported bill 0.01.
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r40i1b",
                    1,
                    Some("s1"),
                    1,
                    None,
                    Some("0.01"),
                    "reported"
                )
            )
            .unwrap()
        );
        // A proven turn of another native session s2, priced 0.02.
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r40i1b".into(),
                    seq: 2,
                    profile_id: Some("pvp_r40".into()),
                    model: Some("m-r40".into()),
                    scope: "turn".into(),
                    scope_id: Some("T-other".into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: Some("s2".into()),
                    total_tokens: Some(200),
                    input_tokens: Some(200),
                    output_tokens: Some(0),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: Some("0.02".into()),
                    accounting: "estimated".into(),
                    observed_at: "2026-10-15T00:00:00.000Z".into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );
        // An unqualified priced turn — cannot be distinguished from the
        // reported session, so its 0.04 estimate stands down.
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r40i1b".into(),
                    seq: 3,
                    profile_id: Some("pvp_r40".into()),
                    model: Some("m-r40".into()),
                    scope: "turn".into(),
                    scope_id: Some("T-unq".into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: None,
                    total_tokens: Some(400),
                    input_tokens: Some(400),
                    output_tokens: Some(0),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: Some("0.04".into()),
                    accounting: "estimated".into(),
                    observed_at: "2026-10-15T00:00:00.000Z".into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );

        let inst = aggregate_instance(&conn, "ins_r40i1b").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert!(
            (inst.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "reported s1 0.01 + proven-other-session turn 0.02, unqualified 0.04 stands down"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd);
        // Token fold is independent: both turns' tokens survive the bill.
        assert_eq!(inst.input_tokens, Some(600));
        assert_eq!(inst.input_tokens, supply.input_tokens);
    }

    #[test]
    fn another_sessions_bill_keeps_an_unpriced_qualified_turns_unknown_cost() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r41i4",
                    1,
                    Some("s1"),
                    1,
                    None,
                    Some("0.01"),
                    "reported"
                )
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r41i4".into(),
                    seq: 2,
                    profile_id: Some("pvp_r40".into()),
                    model: Some("m-r40".into()),
                    scope: "turn".into(),
                    scope_id: Some("T2".into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: Some("s2".into()),
                    total_tokens: Some(200),
                    input_tokens: Some(200),
                    output_tokens: Some(0),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                    accounting: "estimated".into(),
                    observed_at: "2026-10-15T00:00:00.000Z".into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );

        let inst = aggregate_instance(&conn, "ins_r41i4").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert_eq!(
            inst.cost_usd, None,
            "S2's Unknown price poisons past S1's bill"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd, "instance/supply parity");
        assert_eq!(
            inst.input_tokens,
            Some(200),
            "the S2 turn tokens still count"
        );
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Unknown);
        assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Unknown);
    }

    #[test]
    fn supply_excludes_an_estimate_covered_by_a_reported_bill_under_another_model() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Reported S session under model B, rev1, tokenless $0.03.
        let mut reported_s =
            r40_session_row("ins_r41i5", 1, Some("S"), 1, None, Some("0.03"), "reported");
        reported_s.profile_id = Some("pvp_r41".into());
        reported_s.model = Some("m-b".into());
        assert!(insert_usage_event(&conn, &reported_s).unwrap());
        // Turn T under model A, rev2, provenance S, tokens + est $0.02. The
        // higher revision keeps T outside stock coverage in the token fold.
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r41i5".into(),
                    seq: 2,
                    profile_id: Some("pvp_r41".into()),
                    model: Some("m-a".into()),
                    scope: "turn".into(),
                    scope_id: Some("T".into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 2,
                    source_session_id: Some("S".into()),
                    total_tokens: Some(200),
                    input_tokens: Some(200),
                    output_tokens: Some(0),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: Some("0.02".into()),
                    accounting: "estimated".into(),
                    observed_at: "2026-10-15T00:00:00.000Z".into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );

        // Instance: S's reported bill alone.
        let inst = aggregate_instance(&conn, "ins_r41i5").unwrap();
        assert!((inst.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(inst.input_tokens, Some(200), "tokens come from T");

        // Supply(A): T's covered estimate is excluded (no rebill); the turn
        // still gives the model token evidence.
        let supply_a = aggregate_supply(&conn, "pvp_r41", "m-a").unwrap();
        assert_eq!(supply_a.input_tokens, Some(200));
        assert_eq!(
            supply_a.cost_usd,
            Some(0.0),
            "a bill under B covers S's estimate in supply(A)"
        );
        assert_eq!(
            supply_a.budget_status(Some(0.015)),
            BudgetStatus::Ok,
            "the covered 0.02 estimate never Stops a model-A budget"
        );

        // Supply(B): the reported bill; the A-model turn is out of domain.
        let supply_b = aggregate_supply(&conn, "pvp_r41", "m-b").unwrap();
        assert!((supply_b.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(supply_b.input_tokens, None);
        assert_eq!(supply_b.budget_status(Some(0.02)), BudgetStatus::Stop);

        // Instance/supply parity where the domains see the same retained data.
        assert_eq!(inst.cost_usd, supply_b.cost_usd);
    }

    #[test]
    fn a_reported_bill_never_removes_an_eligible_messages_tokens() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Eligible Message M linked to turn T; NO Turn row exists, so it is
        // live per-call flow: input100, unpriced.
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r40i3".into(),
                    seq: 1,
                    profile_id: Some("pvp_r40".into()),
                    model: Some("m-r40".into()),
                    scope: "message".into(),
                    scope_id: Some("r1".into()),
                    turn_id: Some("T".into()),
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: None,
                    total_tokens: Some(100),
                    input_tokens: Some(100),
                    output_tokens: Some(0),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                    accounting: "estimated".into(),
                    observed_at: "2026-10-15T00:00:00.000Z".into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );
        // Tokenless reported session bill for the same instance.
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r40i3",
                    2,
                    Some("s1"),
                    1,
                    None,
                    Some("0.01"),
                    "reported"
                )
            )
            .unwrap()
        );

        let inst = aggregate_instance(&conn, "ins_r40i3").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert_eq!(
            inst.input_tokens,
            Some(100),
            "Message tokens survive the bill"
        );
        assert_eq!(inst.total_tokens, Some(100));
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(inst.total_tokens, supply.total_tokens);
        assert!((inst.cost_usd.unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(inst.cost_usd, supply.cost_usd);
        // The rollup still observes the one Message call as a turn.
        let rollup = rollup_fold(&conn, "ins_r40i3", "codex", Some("m-r40"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.turns, 1, "the Message call stays an observed turn");
        assert_eq!(rollup.session_input_tokens, Some(100));
    }

    #[test]
    fn a_reported_stock_is_not_added_to_newer_uncovered_turn_flow() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let turn = |seq: i64, scope: &str, revision: i64, input: i64| UsageEventRow {
            instance_id: "ins_r40i4".into(),
            seq,
            profile_id: Some("pvp_r40".into()),
            model: Some("m-r40".into()),
            scope: "turn".into(),
            scope_id: Some(scope.into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: revision,
            source_session_id: Some("S".into()),
            total_tokens: Some(input),
            input_tokens: Some(input),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: "2026-10-15T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        };
        assert!(insert_usage_event(&conn, &turn(1, "T1", 1, 100)).unwrap());
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r40i4",
                    2,
                    Some("S"),
                    1,
                    Some(100),
                    Some("0.01"),
                    "reported"
                )
            )
            .unwrap()
        );
        assert!(insert_usage_event(&conn, &turn(3, "T2", 2, 20)).unwrap());

        let inst = aggregate_instance(&conn, "ins_r40i4").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert_eq!(
            inst.input_tokens,
            Some(120),
            "uncovered turns 100 + 20, the older reported stock is not added"
        );
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(inst.total_tokens, supply.total_tokens);
        assert!((inst.cost_usd.unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(inst.cost_usd, supply.cost_usd);
        let rollup = rollup_fold(&conn, "ins_r40i4", "codex", Some("m-r40"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(120));
    }

    #[test]
    fn supply_keeps_a_legacy_reported_bill_after_a_later_costless_frame() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Newest-KNOWN legacy reported bill: 0.07, Unknown token buckets.
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row("ins_r40i5", 1, None, 1, None, Some("0.07"), "reported")
            )
            .unwrap()
        );
        // Later costless/window-only NULL-scope point (no known value moved).
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row("ins_r40i5", 2, None, 1, None, None, "reported")
            )
            .unwrap(),
            "a window-only point with no known cost is accepted"
        );
        // The newest legacy row is the costless one; the bill must still be
        // the newest-KNOWN 0.07.
        let legacy_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = 'ins_r40i5' AND scope = 'session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_rows, 2);

        let inst = aggregate_instance(&conn, "ins_r40i5").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert!(
            (inst.cost_usd.unwrap() - 0.07).abs() < 1e-9,
            "instance keeps the newest-KNOWN legacy bill past a costless frame"
        );
        assert_eq!(
            inst.cost_usd, supply.cost_usd,
            "supply applies the same legacy bill as the instance fold"
        );
        assert_eq!(inst.total_tokens, supply.total_tokens);
        assert_eq!(inst.input_tokens, supply.input_tokens);
    }

    #[test]
    fn a_retained_model_bill_stays_visible_when_its_frame_lost_token_selection() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    ..r40_session_row(
                        "ins_r40i6",
                        1,
                        Some("S"),
                        1,
                        Some(100),
                        Some("0.01"),
                        "reported"
                    )
                }
            )
            .unwrap()
        );
        // Model switch: the same session's later snapshot is projected under
        // model B (the Hub projects with the instance's current model).
        let mut switched = r40_session_row(
            "ins_r40i6",
            2,
            Some("S"),
            2,
            Some(200),
            Some("0.03"),
            "reported",
        );
        switched.model = Some("m-r40-b".into());
        switched.source_session_id = Some("S".into());
        assert!(insert_usage_event(&conn, &switched).unwrap());

        // B owns the selected token stock and the newest bill.
        let inst = aggregate_instance(&conn, "ins_r40i6").unwrap();
        let supply_b = aggregate_supply(&conn, "pvp_r40", "m-r40-b").unwrap();
        assert_eq!(supply_b.input_tokens, Some(200));
        assert!((supply_b.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(inst.input_tokens, supply_b.input_tokens);
        assert_eq!(inst.cost_usd, supply_b.cost_usd);
        // A stays VISIBLE after the /model switch (AA3 provider presence),
        // but r43 AC4 bills the cumulative session meter ONCE: the newer B
        // bill supersedes A's 0.01, so supply(A) contributes presence with a
        // known zero amount — summing supplies equals the instance aggregate.
        let supply_a = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert!(supply_a.events > 0, "model A stays a visible provider row");
        assert_eq!(
            supply_a.cost_usd,
            Some(0.0),
            "A's superseded cumulative bill is not re-summed"
        );
        assert_eq!(
            supply_a.total_tokens, None,
            "A's snapshot is not selected token stock"
        );
        assert_eq!(supply_a.input_tokens, None);
        assert_eq!(supply_a.output_tokens, None);
        assert_eq!(
            supply_a.budget_status(Some(0.005)),
            BudgetStatus::Ok,
            "the cumulative bill lives on B; A's budget is not stopped twice"
        );
    }

    fn r42_turn(
        instance: &str,
        seq: i64,
        scope: &str,
        sid: &str,
        revision: i64,
        input: i64,
        cost: Option<&str>,
    ) -> UsageEventRow {
        UsageEventRow {
            instance_id: instance.into(),
            seq,
            profile_id: Some("pvp_r42".into()),
            model: Some("m-r42".into()),
            scope: "turn".into(),
            scope_id: Some(scope.into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: revision,
            source_session_id: Some(sid.into()),
            total_tokens: Some(input),
            input_tokens: Some(input),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: cost.map(str::to_string),
            accounting: "estimated".into(),
            observed_at: "2026-10-16T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        }
    }

    #[test]
    fn a_tokenless_reported_carrier_never_certifies_turn_coverage() {
        for carrier_revision in [1i64, 2] {
            let label = if carrier_revision == 1 {
                "equal"
            } else {
                "newer"
            };
            let conn = Connection::open_in_memory().unwrap();
            migrate(&conn).unwrap();
            assert!(
                insert_usage_event(&conn, &r42_turn("ins_r42i1", 1, "T1", "s1", 1, 100, None))
                    .unwrap()
            );
            // Tokenless reported carrier on the SAME profile/model as the
            // turns (model-attributed bill stays model-scoped in supply).
            let mut carrier = r40_session_row(
                "ins_r42i1",
                2,
                Some("s1"),
                carrier_revision,
                None,
                Some("0.01"),
                "reported",
            );
            carrier.profile_id = Some("pvp_r42".into());
            carrier.model = Some("m-r42".into());
            assert!(
                insert_usage_event(&conn, &carrier).unwrap(),
                "{label}: tokenless reported carrier is accepted"
            );
            assert!(
                insert_usage_event(
                    &conn,
                    &r42_turn("ins_r42i1", 3, "T2", "s2", 1, 200, Some("0.02"))
                )
                .unwrap()
            );

            let inst = aggregate_instance(&conn, "ins_r42i1").unwrap();
            let supply = aggregate_supply(&conn, "pvp_r42", "m-r42").unwrap();
            assert_eq!(
                inst.input_tokens,
                Some(300),
                "{label}: T1's 100 + T2's 200, the tokenless carrier certifies nothing"
            );
            assert_eq!(inst.total_tokens, Some(300));
            assert_eq!(inst.input_tokens, supply.input_tokens, "{label} parity");
            assert_eq!(inst.total_tokens, supply.total_tokens);
            assert!(
                (inst.cost_usd.unwrap() - 0.03).abs() < 1e-9,
                "{label}: S1 bill 0.01 + S2 estimate 0.02, got {:?}",
                inst.cost_usd
            );
            assert_eq!(inst.cost_usd, supply.cost_usd);
            assert_eq!(
                inst.budget_status(Some(0.02)),
                BudgetStatus::Stop,
                "{label}"
            );
            assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Stop);
        }
    }

    #[test]
    fn a_tokenless_costless_reported_carrier_keeps_its_unpriced_turn_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // S1 carries the priced bill.
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r42i1c",
                    1,
                    Some("s1"),
                    1,
                    None,
                    Some("0.01"),
                    "reported"
                )
            )
            .unwrap()
        );
        // S2: unpriced qualified Turn ...
        assert!(
            insert_usage_event(&conn, &r42_turn("ins_r42i1c", 2, "T2", "s2", 1, 200, None))
                .unwrap()
        );
        // ... and a tokenless, costless REPORTED session carrier at the same
        // revision (an accepted stored shape).
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row("ins_r42i1c", 3, Some("s2"), 1, None, None, "reported")
            )
            .unwrap()
        );

        let inst = aggregate_instance(&conn, "ins_r42i1c").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r42", "m-r42").unwrap();
        assert_eq!(inst.input_tokens, Some(200), "the S2 turn tokens survive");
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(
            inst.cost_usd, None,
            "S2's uncovered unpriced turn poisons past the costless carrier and S1's bill"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd);
        assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Unknown);
        assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Unknown);
    }

    #[test]
    fn a_tokenless_reported_carrier_does_not_displace_earlier_token_stock() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut s1_est =
            r40_session_row("ins_r43i2a", 1, Some("s1"), 1, Some(100), None, "estimated");
        s1_est.profile_id = Some("pvp_r43".into());
        s1_est.model = Some("m-r43".into());
        assert!(insert_usage_event(&conn, &s1_est).unwrap());
        let mut s1_bill = r40_session_row(
            "ins_r43i2a",
            2,
            Some("s1"),
            2,
            None,
            Some("0.01"),
            "reported",
        );
        s1_bill.profile_id = Some("pvp_r43".into());
        s1_bill.model = Some("m-r43".into());
        assert!(insert_usage_event(&conn, &s1_bill).unwrap());
        let mut s2 = r40_session_row(
            "ins_r43i2a",
            3,
            Some("s2"),
            1,
            Some(200),
            Some("0.02"),
            "estimated",
        );
        s2.profile_id = Some("pvp_r43".into());
        s2.model = Some("m-r43".into());
        assert!(insert_usage_event(&conn, &s2).unwrap());

        let inst = aggregate_instance(&conn, "ins_r43i2a").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r43", "m-r43").unwrap();
        assert_eq!(
            inst.input_tokens,
            Some(300),
            "S1's 100 survives its tokenless bill"
        );
        assert_eq!(inst.total_tokens, Some(300));
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(inst.total_tokens, supply.total_tokens);
        assert!(
            (inst.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "bill 0.01 + est 0.02"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd);
        assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Stop);
        assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Stop);
    }

    #[test]
    fn a_tokenless_costless_stock_successor_keeps_the_earlier_unknown_cost() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let row =
            |seq: i64, sid: &str, input: Option<i64>, cost: Option<&str>, accounting: &str| {
                let mut r =
                    r40_session_row("ins_r43i2b", seq, Some(sid), seq, input, cost, accounting);
                r.profile_id = Some("pvp_r43".into());
                r.model = Some("m-r43".into());
                r
            };
        assert!(insert_usage_event(&conn, &row(1, "s1", None, Some("0.01"), "reported")).unwrap());
        assert!(insert_usage_event(&conn, &row(2, "s2", Some(200), None, "estimated")).unwrap());
        assert!(insert_usage_event(&conn, &row(3, "s2", None, None, "reported")).unwrap());

        let inst = aggregate_instance(&conn, "ins_r43i2b").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r43", "m-r43").unwrap();
        assert_eq!(
            inst.input_tokens,
            Some(200),
            "S2's 200-token stock survives"
        );
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(
            inst.cost_usd, None,
            "S2's uncovered Unknown cost is preserved"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd);
        assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Unknown);
        assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Unknown);
    }

    #[test]
    fn a_bill_only_session_rolls_up_unknown_tokens_not_zero() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut bill = r40_session_row(
            "ins_r43i3",
            1,
            Some("s1"),
            1,
            None,
            Some("0.01"),
            "reported",
        );
        bill.profile_id = Some("pvp_r43".into());
        bill.model = Some("m-r43".into());
        assert!(insert_usage_event(&conn, &bill).unwrap());

        let inst = aggregate_instance(&conn, "ins_r43i3").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r43", "m-r43").unwrap();
        assert_eq!(inst.total_tokens, None);
        assert_eq!(inst.input_tokens, None);
        assert_eq!(inst.output_tokens, None);
        assert_eq!(inst.cost_usd, Some(0.01));

        let rollup = rollup_fold(&conn, "ins_r43i3", "codex", Some("m-r43"))
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.session_input_tokens, None,
            "bill-only Session: rollup Unknown, not a fabricated zero"
        );
        assert_eq!(rollup.session_output_tokens, None);
        assert_eq!(rollup.cache_read_tokens, None);
        assert_eq!(rollup.cache_creation_tokens, None);
        // Parity with the aggregate.
        assert_eq!(rollup.session_input_tokens, inst.input_tokens);
        assert_eq!(rollup.session_input_tokens, supply.input_tokens);
        assert_eq!(inst.input_tokens, supply.input_tokens);
    }

    struct R43Rng(u64);
    impl R43Rng {
        fn next_u64(&mut self) -> u64 {
            // MMIX constants.
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> usize {
            (self.next_u64() % n) as usize
        }
        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64)]
        }
    }

    /// Canonical FULL-row fingerprint of EVERY usage column, including
    /// lineage_state. AE2: replayed rows are live rows column for column, so a
    /// recovered database must match its independent live reference on all 21
    /// columns.
    fn r43_dump_rows(conn: &Connection) -> Vec<Vec<Option<String>>> {
        conn.prepare(
            "SELECT instance_id, seq, profile_id, model, scope, scope_id, turn_id,
                    mode, total_tokens, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                    observed_at, observed_at_source, metric_revision,
                    source_session_id, native_context_window, lineage_state
             FROM usage_events
             ORDER BY 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21",
        )
        .unwrap()
        .query_map([], |r| {
            (0..21)
                .map(|i| {
                    Ok(match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => None,
                        rusqlite::types::ValueRef::Integer(v) => Some(v.to_string()),
                        rusqlite::types::ValueRef::Real(v) => Some(v.to_string()),
                        rusqlite::types::ValueRef::Text(v) => {
                            Some(String::from_utf8_lossy(v).into_owned())
                        }
                        rusqlite::types::ValueRef::Blob(v) => Some(format!("<blob {}>", v.len())),
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    #[derive(Clone, Copy)]
    enum Basket {
        Known(i64),
        NotApplicable,
        Unknown,
    }
    impl Basket {
        fn decoded(self) -> Option<i64> {
            match self {
                Basket::Known(v) => Some(v),
                Basket::NotApplicable => Some(0),
                Basket::Unknown => None,
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn r44_wire_record(
        instance: &str,
        seq: i64,
        scope: &str,
        scope_id: Option<&str>,
        session: Option<&str>,
        turn: Option<&str>,
        revision: u64,
        accounting: &str,
        baskets: [Basket; 5], // total, input, output, cacheRead, cacheWrite
        cost: Option<&str>,   // None -> Knowledge::Unknown
        window: Option<u64>,
        native_at: Option<&str>,
    ) -> JournalRecord {
        use remuda_protocol::{
            Accounting, Cost, Id, InputAccounting, Knowledge, U64, UsageMode, UsagePayload,
            UsageScope,
        };
        let k = |b: Basket| -> Knowledge<U64> {
            match b {
                Basket::Known(v) => Knowledge::Known {
                    value: U64(v as u64),
                },
                Basket::NotApplicable => Knowledge::NotApplicable,
                Basket::Unknown => Knowledge::Unknown {
                    reason: "unreported".into(),
                    evidence_event_ids: vec![],
                },
            }
        };
        let [total, input, output, cache_read, cache_write] = baskets;
        let cost_knowledge: Knowledge<Cost> = match cost {
            Some(amount) => Knowledge::Known {
                value: Cost {
                    amount: amount.into(),
                    currency: "USD".into(),
                },
            },
            None => Knowledge::Unknown {
                reason: "unpriced".into(),
                evidence_event_ids: vec![],
            },
        };
        let scope_enum = match scope {
            "message" => UsageScope::Message,
            "session" => UsageScope::Session,
            _ => UsageScope::Turn,
        };
        let native_key = scope_id.unwrap_or("legacy");
        let usage_id = Id::derive(
            "obj",
            instance,
            &format!("usage:{scope}:{native_key}:{seq}"),
        )
        .unwrap_or_else(|_| Id::new("obj").unwrap());
        let payload = UsagePayload {
            usage_id,
            scope: scope_enum,
            scope_id: scope_id.unwrap_or("").to_string(),
            mode: UsageMode::Snapshot,
            metric_revision: U64(revision),
            source_session_id: session.map(str::to_string),
            input_tokens: k(input),
            input_accounting: InputAccounting::Uncached,
            output_tokens: k(output),
            reasoning_tokens: Knowledge::Unknown {
                reason: "unreported".into(),
                evidence_event_ids: vec![],
            },
            cache_read_tokens: k(cache_read),
            cache_write_tokens: k(cache_write),
            total_tokens: k(total),
            cost: cost_knowledge,
            accounting: if accounting == "reported" {
                Accounting::Reported
            } else {
                Accounting::Estimated
            },
            native_fields_ref: None,
            context_window: window.map(U64),
        };
        // Build the FULL typed observation envelope — schema version, event/
        // journal/instance/host ids and complete source metadata — then
        // serialize it. Never hand-assemble a partial envelope here.
        use remuda_protocol::{
            Completeness, DriverKind, EventId, HostId, InstanceId, NativeRequestKey, Observation,
            ObservationPayload, ObservationSource, RuntimeCursor, SchemaVersion, SourceChannel,
            SourceCursor, SourceDelivery, Timestamp,
        };
        let inst_branded = InstanceId::try_from(String::from(
            Id::derive("ins", "remuda-hub-usage-tests", instance).unwrap(),
        ))
        .unwrap();
        let journal_id =
            Id::derive("obj", instance, "journal").unwrap_or_else(|_| Id::new("obj").unwrap());
        let host_id =
            HostId::try_from(String::from(Id::derive("hst", instance, "host").unwrap())).unwrap();
        let event_id = EventId::try_from(String::from(
            Id::derive(
                "evt",
                instance,
                &format!("usage:{scope}:{native_key}:{seq}"),
            )
            .unwrap(),
        ))
        .unwrap();
        let stamp = native_at.unwrap_or("2026-10-20T00:00:00.000Z");
        let observed_at = Timestamp::try_from(stamp.to_string()).unwrap();
        let native_at_knowledge = match native_at {
            Some(at) => Knowledge::Known {
                value: Timestamp::try_from(at.to_string()).unwrap(),
            },
            None => Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: vec![],
            },
        };
        let known_s = |v: &str| Knowledge::Known {
            value: v.to_string(),
        };
        let unknown_s = || Knowledge::Unknown {
            reason: "not-emitted".into(),
            evidence_event_ids: vec![],
        };
        let source = ObservationSource {
            driver_kind: DriverKind::CodexAppserver,
            driver_version: "r46-test".into(),
            adapter_version: "r46-test".into(),
            channel: SourceChannel::Hook,
            delivery: SourceDelivery::Live,
            native_session_id: session.map(known_s).unwrap_or_else(unknown_s),
            native_turn_id: turn.map(known_s).unwrap_or_else(unknown_s),
            native_agent_id: unknown_s(),
            native_item_id: unknown_s(),
            native_event_id: unknown_s(),
            native_request_id: NativeRequestKey::None,
            source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                ledger_revision: U64(0),
            })),
        };
        let obs = Observation {
            schema_version: SchemaVersion,
            event_id,
            journal_id,
            instance_id: inst_branded,
            run_id: None,
            host_id,
            process_generation: U64(1),
            run_generation: None,
            seq: U64(seq as u64),
            observed_at,
            native_at: native_at_knowledge,
            source,
            completeness: Completeness::Structured,
            raw_ref: None,
            evidence_event_ids: vec![],
            body: ObservationPayload::Usage(Box::new(payload)),
        };
        let mut event = serde_json::to_value(&obs).unwrap();
        // Historical NULL-scope variant: serialize the FULL typed payload
        // first (scopeId comes out as ""), then REMOVE the key — never pass an
        // empty string, which the projector reads as a real scoped id.
        if scope_id.is_none()
            && let Some(payload_obj) = event.get_mut("payload").and_then(Value::as_object_mut)
        {
            payload_obj.remove("scopeId");
        }
        JournalRecord {
            instance_id: instance.into(),
            seq,
            event_id: String::from(obs.event_id.as_id().clone()),
            event,
            observed_at: stamp.into(),
        }
    }

    struct R44Event {
        model: &'static str,
        legacy: bool,
        record: JournalRecord,
    }

    #[derive(Clone, Copy, Default)]
    struct R44Features {
        any_data: bool,
        saw_null_scope: bool,
        saw_message_revision: bool,
    }

    fn r44_property_one(seed: u64) -> R44Features {
        let mut saw_null_scope = false;
        let mut saw_message_revision = false;
        let mut rng = R43Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15));
        let profile = "pvp_prop";
        let models = ["m-a", "m-b"];
        let draw_basket = |rng: &mut R43Rng| -> Basket {
            match rng.below(3) {
                0 => Basket::Known(100 * (1 + rng.below(5)) as i64),
                1 => Basket::NotApplicable,
                _ => Basket::Unknown,
            }
        };
        let cost_of = |rng: &mut R43Rng| -> Option<String> {
            if rng.below(4) > 0 {
                Some(format!("{:.2}", 0.01 * (1 + rng.below(50)) as f64))
            } else {
                None
            }
        };

        // The seed DB starts on the r28 FALLBACK shape when any instance
        // begins legacy; its first events are written through the 14-column
        // path (NULL scope) and recovered when the schema is upgraded.
        let n_instances = 1 + rng.below(3);
        let begin_legacy = rng.below(3) == 0;
        let legacy_instances: Vec<bool> = (0..n_instances)
            .map(|_| begin_legacy && rng.below(2) == 0)
            .collect();
        let any_legacy = legacy_instances.iter().any(|x| *x);
        let conn = Connection::open_in_memory().unwrap();
        if any_legacy {
            create_r28_usage_table_with_turn_index(&conn);
        } else {
            migrate(&conn).unwrap();
        }
        create_journal_table(&conn);

        // Per instance: sessions, accepted turn lineages (scope, sid, revision)
        // and the event list.
        let mut sessions: Vec<Vec<String>> = Vec::new();
        let mut lineages: Vec<Vec<(String, String, u64)>> = Vec::new();
        // Durable Messages that may receive later REVISIONS on the same marker
        // (the (i) failure shape: a later revision shares the earlier frame's
        // physical row through the live Message upsert). (marker, turn, rev)
        let mut messages: Vec<Vec<(String, Option<String>, u64)>> = Vec::new();
        let mut per_instance: Vec<Vec<R44Event>> = (0..n_instances).map(|_| Vec::new()).collect();
        for ii in 0..n_instances {
            let n_sessions = 1 + rng.below(3);
            sessions.push((0..n_sessions).map(|s| format!("sess-{ii}-{s}")).collect());
            lineages.push(Vec::new());
            messages.push(Vec::new());
            let n_events = 6 + rng.below(14);
            let legacy_cut = if legacy_instances[ii] {
                1 + rng.below(n_events as u64)
            } else {
                0
            };
            for ei in 0..n_events {
                let seq = ei as i64 + 1;
                let model = models[rng.below(models.len() as u64)];
                let inst = format!("ins_prop_{ii}");
                let stamp = format!("2026-10-20T00:00:{:02}.000Z", seq % 60);
                let legacy = legacy_instances[ii] && ei < legacy_cut;
                let kind = *rng.pick(&[
                    "turn",
                    "turn_first_tokenless",
                    "turn_revise",
                    "turn_correct",
                    "session",
                    "message",
                ]);
                let record = match kind {
                    "turn" | "turn_first_tokenless" => {
                        let sid = sessions[ii][rng.below(sessions[ii].len() as u64)].clone();
                        let scope = format!("turn-{seq}");
                        let revision = 1 + rng.below(3) as u64;
                        let first_tokenless = kind == "turn_first_tokenless";
                        let baskets = if first_tokenless {
                            // FIRST frame, no baskets: bill + window + native
                            // time. Recovery must read it as an initial frame
                            // (recover metadata), never a cost-only correction.
                            [Basket::Unknown; 5]
                        } else {
                            let input = draw_basket(&mut rng);
                            let output = draw_basket(&mut rng);
                            let cr = draw_basket(&mut rng);
                            let cw = draw_basket(&mut rng);
                            let total = match (
                                input.decoded(),
                                output.decoded(),
                                cr.decoded(),
                                cw.decoded(),
                            ) {
                                (Some(a), Some(o), Some(r), Some(w)) if rng.below(2) == 0 => {
                                    Basket::Known(a + o + r + w)
                                }
                                _ => Basket::Unknown,
                            };
                            [total, input, output, cr, cw]
                        };
                        let window = if first_tokenless || rng.below(4) == 0 {
                            Some(200_000)
                        } else {
                            None
                        };
                        let cost = if first_tokenless || rng.below(3) > 0 {
                            cost_of(&mut rng)
                        } else {
                            None
                        };
                        let native = if first_tokenless || rng.below(2) == 0 {
                            Some(stamp.as_str())
                        } else {
                            None
                        };
                        lineages[ii].push((scope.clone(), sid.clone(), revision));
                        r44_wire_record(
                            &inst,
                            seq,
                            "turn",
                            Some(&scope),
                            Some(&sid),
                            Some(&scope),
                            revision,
                            "estimated",
                            baskets,
                            cost.as_deref(),
                            window,
                            native,
                        )
                    }
                    "turn_revise" => {
                        if lineages[ii].is_empty() {
                            // Nothing to revise: fall back to a fresh turn.
                            let sid = sessions[ii][rng.below(sessions[ii].len() as u64)].clone();
                            let scope = format!("turn-{seq}");
                            let input = draw_basket(&mut rng);
                            let output = draw_basket(&mut rng);
                            let cr = draw_basket(&mut rng);
                            let cw = draw_basket(&mut rng);
                            let total = match (
                                input.decoded(),
                                output.decoded(),
                                cr.decoded(),
                                cw.decoded(),
                            ) {
                                (Some(a), Some(o), Some(r), Some(w)) if rng.below(2) == 0 => {
                                    Basket::Known(a + o + r + w)
                                }
                                _ => Basket::Unknown,
                            };
                            let revision = 1 + rng.below(3) as u64;
                            lineages[ii].push((scope.clone(), sid.clone(), revision));
                            r44_wire_record(
                                &inst,
                                seq,
                                "turn",
                                Some(&scope),
                                Some(&sid),
                                Some(&scope),
                                revision,
                                "estimated",
                                [total, input, output, cr, cw],
                                cost_of(&mut rng).as_deref(),
                                if rng.below(4) == 0 {
                                    Some(200_000)
                                } else {
                                    None
                                },
                                if rng.below(2) == 0 {
                                    Some(stamp.as_str())
                                } else {
                                    None
                                },
                            )
                        } else {
                            // COUNTER-BEARING revision of an EXISTING lineage:
                            // same scope/sid, strictly higher revision, real
                            // baskets; window/nativeAt usually omitted so the
                            // revision must retain the first frame's metadata.
                            let idx = rng.below(lineages[ii].len() as u64);
                            let (scope, sid, rev) = lineages[ii][idx].clone();
                            let input = draw_basket(&mut rng);
                            let output = draw_basket(&mut rng);
                            let cr = draw_basket(&mut rng);
                            let cw = draw_basket(&mut rng);
                            let total = match (
                                input.decoded(),
                                output.decoded(),
                                cr.decoded(),
                                cw.decoded(),
                            ) {
                                (Some(a), Some(o), Some(r), Some(w)) if rng.below(2) == 0 => {
                                    Basket::Known(a + o + r + w)
                                }
                                _ => Basket::Unknown,
                            };
                            let new_rev = rev + 1;
                            lineages[ii][idx] = (scope.clone(), sid.clone(), new_rev);
                            r44_wire_record(
                                &inst,
                                seq,
                                "turn",
                                Some(&scope),
                                Some(&sid),
                                Some(&scope),
                                new_rev,
                                "estimated",
                                [total, input, output, cr, cw],
                                cost_of(&mut rng).as_deref(),
                                if rng.below(5) == 0 {
                                    Some(200_000)
                                } else {
                                    None
                                },
                                if rng.below(5) == 0 {
                                    Some(stamp.as_str())
                                } else {
                                    None
                                },
                            )
                        }
                    }
                    "turn_correct" => {
                        if lineages[ii].is_empty() {
                            // No lineage yet: emit an ordinary turn instead.
                            let sid = sessions[ii][rng.below(sessions[ii].len() as u64)].clone();
                            let scope = format!("turn-{seq}");
                            let input = draw_basket(&mut rng);
                            let revision = 1 + rng.below(3) as u64;
                            lineages[ii].push((scope.clone(), sid.clone(), revision));
                            r44_wire_record(
                                &inst,
                                seq,
                                "turn",
                                Some(&scope),
                                Some(&sid),
                                Some(&scope),
                                revision,
                                "estimated",
                                [
                                    Basket::Unknown,
                                    input,
                                    Basket::NotApplicable,
                                    Basket::Unknown,
                                    Basket::Unknown,
                                ],
                                cost_of(&mut rng).as_deref(),
                                None,
                                None,
                            )
                        } else {
                            // Strictly-newer cost-only correction: every
                            // additive basket explicitly Unknown. It may carry
                            // window/native time which must be suppressed.
                            let idx = rng.below(lineages[ii].len() as u64);
                            // A correction stays in the SAME proven lineage.
                            let (scope, sid, rev) = lineages[ii][idx].clone();
                            let correction_rev = rev + 1 + rng.below(2) as u64;
                            let window = if rng.below(2) == 0 {
                                Some(1_000_000)
                            } else {
                                None
                            };
                            let native = if rng.below(2) == 0 {
                                Some(stamp.as_str())
                            } else {
                                None
                            };
                            lineages[ii][idx] = (scope.clone(), sid.clone(), correction_rev);
                            r44_wire_record(
                                &inst,
                                seq,
                                "turn",
                                Some(&scope),
                                Some(&sid),
                                Some(&scope),
                                correction_rev,
                                "estimated",
                                [Basket::Unknown; 5],
                                cost_of(&mut rng).as_deref(),
                                window,
                                native,
                            )
                        }
                    }
                    "session" => {
                        let sid = sessions[ii][rng.below(sessions[ii].len() as u64)].clone();
                        let reported = rng.below(2) == 0;
                        let accounting = if reported { "reported" } else { "estimated" };
                        let tokenless = rng.below(2) == 0;
                        let revision = 1 + rng.below(3) as u64;
                        let baskets = if tokenless {
                            [Basket::Unknown; 5]
                        } else {
                            let input = draw_basket(&mut rng);
                            [
                                input,
                                input,
                                Basket::NotApplicable,
                                Basket::NotApplicable,
                                Basket::NotApplicable,
                            ]
                        };
                        if legacy {
                            // Legacy NULL-scope session: the typed payload is
                            // serialized whole and scopeId is then REMOVED (an
                            // omission), never emitted as "". It must project
                            // to scope_id = None.
                            let record = r44_wire_record(
                                &inst,
                                seq,
                                "session",
                                None,
                                None,
                                None,
                                revision,
                                accounting,
                                baskets,
                                cost_of(&mut rng).as_deref(),
                                None,
                                Some(stamp.as_str()),
                            );
                            assert!(
                                record.event["payload"].get("scopeId").is_none(),
                                "seed {seed}: legacy variant OMITS scopeId, not \"\""
                            );
                            assert_eq!(
                                project_usage_event(&record, Some(profile), Some(model))
                                    .unwrap()
                                    .scope_id,
                                None,
                                "seed {seed}: omitted scopeId projects to None"
                            );
                            saw_null_scope = true;
                            record
                        } else {
                            r44_wire_record(
                                &inst,
                                seq,
                                "session",
                                Some(&sid),
                                Some(&sid),
                                None,
                                revision,
                                accounting,
                                baskets,
                                cost_of(&mut rng).as_deref(),
                                None,
                                Some(stamp.as_str()),
                            )
                        }
                    }
                    "message" => {
                        // Durable Message linked to an existing Turn when one
                        // exists (nativeTurnId); marker lives in scopeId.
                        // Half the time (once one exists) emit a later
                        // REVISION of an earlier durable message: same marker,
                        // strictly higher revision. On the current schema the
                        // live upsert folds it onto the earlier physical row,
                        // the shape the AE2 complete replay must rebuild.
                        let revise = !messages[ii].is_empty() && rng.below(2) == 0;
                        if revise {
                            saw_message_revision = true;
                            let idx = rng.below(messages[ii].len() as u64);
                            let (marker, linked, rev) = messages[ii][idx].clone();
                            let new_rev = rev + 1;
                            messages[ii][idx] = (marker.clone(), linked.clone(), new_rev);
                            let input = Basket::Known((rng.below(5) as i64) * 100);
                            let output = Basket::Known((rng.below(5) as i64) * 100);
                            r44_wire_record(
                                &inst,
                                seq,
                                "message",
                                Some(&marker),
                                None,
                                linked.as_deref(),
                                new_rev,
                                "estimated",
                                [
                                    Basket::Unknown,
                                    input,
                                    output,
                                    Basket::NotApplicable,
                                    Basket::NotApplicable,
                                ],
                                cost_of(&mut rng).as_deref(),
                                None,
                                Some(stamp.as_str()),
                            )
                        } else {
                            let linked = if !lineages[ii].is_empty() && rng.below(2) == 0 {
                                Some(lineages[ii][rng.below(lineages[ii].len() as u64)].0.clone())
                            } else {
                                None
                            };
                            let marker = format!(
                                "_remuda_codex_response:v2:r{seq}-{}",
                                rng.next_u64() % 1000
                            );
                            messages[ii].push((marker.clone(), linked.clone(), 1));
                            let input = Basket::Known((rng.below(5) as i64) * 100);
                            let output = Basket::Known((rng.below(5) as i64) * 100);
                            r44_wire_record(
                                &inst,
                                seq,
                                "message",
                                Some(&marker),
                                None,
                                linked.as_deref(),
                                1,
                                "estimated",
                                [
                                    Basket::Unknown,
                                    input,
                                    output,
                                    Basket::NotApplicable,
                                    Basket::NotApplicable,
                                ],
                                cost_of(&mut rng).as_deref(),
                                None,
                                Some(stamp.as_str()),
                            )
                        }
                    }
                    _ => unreachable!(),
                };
                per_instance[ii].push(R44Event {
                    model,
                    legacy,
                    record,
                });
            }
        }

        // ---- Production ingest ---------------------------------------------
        // 1) every legacy event (NULL-scope fallback path) in per-instance
        //    order, journaled first; 2) schema upgrade; 3) every scoped event
        //    through project_usage_event + the live insert path.
        for events in per_instance.iter() {
            for ev in events.iter().filter(|e| e.legacy) {
                insert_journal_raw(&conn, &ev.record);
                let row = project_usage_event(&ev.record, Some(profile), Some(ev.model))
                    .expect("real usage payload projects");
                let _ = insert_usage_event(&conn, &row).unwrap();
            }
        }
        if any_legacy {
            migrate(&conn).unwrap();
        }
        let mut any_data = false;
        for events in per_instance.iter() {
            for ev in events.iter().filter(|e| !e.legacy) {
                insert_journal_raw(&conn, &ev.record);
                let row = project_usage_event(&ev.record, Some(profile), Some(ev.model))
                    .expect("real usage payload projects");
                if insert_usage_event(&conn, &row).unwrap() {
                    any_data = true;
                }
            }
        }

        // Invariant (c): live == complete replay == second migration, FULL rows.
        // Build a previous-release UPGRADE database by removing the one-time
        // marker, then reopen: the migration replays EVERY usage frame of every
        // backed instance over rows the live upserts already collapsed (later
        // message revisions on the first seq, turn corrections advancing seq).
        let live_rows = r43_dump_rows(&conn);
        conn.execute(
            "DELETE FROM usage_schema_meta WHERE key = 'journal_replay_v1'",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap_or_else(|e| panic!("seed {seed}: replay migrate {e}"));
        let once_rows = r43_dump_rows(&conn);
        migrate(&conn).unwrap_or_else(|e| panic!("seed {seed}: migrate2 {e}"));
        let twice_rows = r43_dump_rows(&conn);
        assert_eq!(
            live_rows, once_rows,
            "seed {seed}: complete replay must reproduce live rows"
        );
        assert_eq!(
            once_rows, twice_rows,
            "seed {seed}: second migration changed rows"
        );

        // Invariant (c, independent): fallback recovery equals an INDEPENDENT
        // live reference — a fresh current-schema database that ingests the
        // SAME journal records through the live path in journal order. This is
        // Ruling AE (recovery IS replay); AE3(d) adds producer era: a frame the
        // seed DB produced before the cutover (ev.legacy) is ingested into the
        // reference with legacy provenance, every later frame as live — exactly
        // the era the forced replay assigns by seq <= cutover.
        let live_ref = Connection::open_in_memory().unwrap();
        migrate(&live_ref).unwrap();
        create_journal_table(&live_ref);
        for events in per_instance.iter() {
            for ev in events.iter() {
                insert_journal_raw(&live_ref, &ev.record);
                let row = project_usage_event(&ev.record, Some(profile), Some(ev.model))
                    .expect("real usage payload projects");
                let era = if ev.legacy {
                    IngestEra::Legacy
                } else {
                    IngestEra::Live
                };
                insert_usage_event_era(&live_ref, &row, era).unwrap();
            }
        }
        assert_eq!(
            r43_dump_rows(&live_ref),
            twice_rows,
            "seed {seed}: fallback recovery equals independent era-aware reference"
        );

        // Grand instance totals over every instance.
        let mut grand_input: Option<i64> = Some(0);
        let mut grand_output: Option<i64> = Some(0);
        let mut grand_total: Option<i64> = Some(0);
        let mut grand_cost: Option<f64> = Some(0.0);
        let add = |acc: Option<i64>, v: Option<i64>| match (acc, v) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        for ii in 0..n_instances {
            let inst = format!("ins_prop_{ii}");
            let agg = aggregate_instance(&conn, &inst).unwrap();
            // AE2 (iv): the recovered aggregate equals the independent live
            // reference's, field for field.
            let ref_agg = aggregate_instance(&live_ref, &inst).unwrap();
            assert_eq!(agg, ref_agg, "seed {seed} {inst} aggregate vs live");
            if agg.events > 0 || agg.token_event_rows > 0 {
                any_data = true;
            }
            // Invariant (b): rollup tokens == aggregate tokens.
            if agg.events > 0 || agg.token_event_rows > 0 || agg.cost_usd.is_some() {
                let rollup = rollup_instance(
                    &conn,
                    &RollupRequest {
                        instance_id: &inst,
                        kind: "codex",
                        spec_model: Some("m-a"),
                        effective_model: None,
                        profile_id: Some(profile),
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(
                    rollup.session_input_tokens, agg.input_tokens,
                    "seed {seed} {inst} input"
                );
                assert_eq!(
                    rollup.session_output_tokens, agg.output_tokens,
                    "seed {seed} {inst} output"
                );
                // The recovered rollup equals the live reference rollup.
                let ref_rollup = rollup_instance(
                    &live_ref,
                    &RollupRequest {
                        instance_id: &inst,
                        kind: "codex",
                        spec_model: Some("m-a"),
                        effective_model: None,
                        profile_id: Some(profile),
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(rollup, ref_rollup, "seed {seed} {inst} rollup vs live");
            }
            if agg.token_event_rows > 0 || agg.ambiguous_events > 0 {
                grand_input = add(grand_input, agg.input_tokens);
                grand_output = add(grand_output, agg.output_tokens);
                grand_total = add(grand_total, agg.total_tokens);
            }
            grand_cost = match (grand_cost, agg.cost_usd) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
        }

        // Invariant (a): grand per-model supply == grand instance totals;
        // Unknown iff any present side is Unknown.
        let mut supply_input: Option<i64> = Some(0);
        let mut supply_output: Option<i64> = Some(0);
        let mut supply_total: Option<i64> = Some(0);
        let mut supply_cost: Option<f64> = Some(0.0);
        for model in models {
            let s = aggregate_supply(&conn, profile, model).unwrap();
            // AE2 (iv): recovered supply equals the independent live reference.
            let ref_s = aggregate_supply(&live_ref, profile, model).unwrap();
            assert_eq!(s, ref_s, "seed {seed} supply {model} vs live");
            if s.events == 0 {
                continue;
            }
            if s.token_event_rows > 0 || s.ambiguous_events > 0 {
                supply_input = add(supply_input, s.input_tokens);
                supply_output = add(supply_output, s.output_tokens);
                supply_total = add(supply_total, s.total_tokens);
            }
            supply_cost = match (supply_cost, s.cost_usd) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
        }
        assert_eq!(grand_input, supply_input, "seed {seed}: input grand sum");
        assert_eq!(grand_output, supply_output, "seed {seed}: output grand sum");
        assert_eq!(grand_total, supply_total, "seed {seed}: total grand sum");
        match (grand_cost, supply_cost) {
            (Some(a), Some(b)) => assert!((a - b).abs() < 1e-6, "seed {seed}: cost {b} vs {a}"),
            (a, b) => assert_eq!(a, b, "seed {seed}: cost Unknown parity"),
        }
        R44Features {
            any_data,
            saw_null_scope,
            saw_message_revision,
        }
    }

    #[test]
    fn property_fold_invariants_hold_across_random_sequences() {
        let seeds: u64 = std::env::var("CTXUSAGE_PROPERTY_SEEDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        assert!(
            seeds >= 100,
            "default r44 run covers at least 100 seeds (~<120 s)"
        );
        let mut with_data = 0;
        let mut null_scope_seeds = Vec::new();
        let mut message_revision_seeds = Vec::new();
        for seed in 0..seeds {
            let f = r44_property_one(seed);
            if f.any_data {
                with_data += 1;
            }
            if f.saw_null_scope {
                null_scope_seeds.push(seed);
            }
            if f.saw_message_revision {
                message_revision_seeds.push(seed);
            }
        }
        assert!(
            with_data as u64 > seeds * 9 / 10,
            "generator produced too few populated seeds: {with_data}/{seeds}"
        );
        assert!(
            !null_scope_seeds.is_empty(),
            "generator must exercise the legacy NULL-scope omission"
        );
        assert!(
            !message_revision_seeds.is_empty(),
            "generator must exercise multi-revision Message chains"
        );
        // Surface concrete seeds so the shapes can be pinned deterministically.
        eprintln!(
            "null-scope seeds (first 8): {:?}; message-revision seeds (first 8): {:?}",
            &null_scope_seeds[..null_scope_seeds.len().min(8)],
            &message_revision_seeds[..message_revision_seeds.len().min(8)]
        );
    }

    #[test]
    fn property_invariant_fixed_seeds() {
        // Deterministic pinned seeds. 4 and 5 each hit BOTH the legacy
        // NULL-scope omission and a multi-revision Message chain; 12 also
        // omits scopeId; 14/100/426 pin general shapes from earlier rounds.
        for seed in [4u64, 5, 12, 14, 100, 426] {
            let f = r44_property_one(seed);
            assert!(f.any_data, "seed {seed} should produce data");
        }
    }

    #[test]
    fn property_pinned_feature_seeds() {
        // Concrete seeds (from a full scan) that deterministically exercise the
        // two named shapes, asserted individually and cheaply.
        let mut null_scope = false;
        for seed in [4u64, 5, 12, 13] {
            null_scope |= r44_property_one(seed).saw_null_scope;
        }
        assert!(
            null_scope,
            "a pinned seed must serialize scopeId as an OMISSION projecting to None"
        );
        let mut message_revision = false;
        for seed in 0..8u64 {
            message_revision |= r44_property_one(seed).saw_message_revision;
        }
        assert!(
            message_revision,
            "a pinned seed must contain a multi-revision Message on one physical row"
        );
        // Seed 4 hits both at once — the r45 high failure shape combined with
        // the legacy NULL-scope variant.
        let both = r44_property_one(4);
        assert!(both.saw_null_scope && both.saw_message_revision);
    }

    #[test]
    fn r44_reported_token_stock_with_unknown_cost_stays_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        // Session s0: token-bearing reported stock, no bill (Unknown cost).
        let r0 = r44_wire_record(
            "i",
            1,
            "session",
            Some("s0"),
            Some("s0"),
            None,
            1,
            "reported",
            [
                Basket::Known(100),
                Basket::Known(100),
                Basket::NotApplicable,
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            None,
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        insert_journal_raw(&conn, &r0);
        let row = project_usage_event(&r0, Some("p"), Some("m")).unwrap();
        insert_usage_event(&conn, &row).unwrap();

        let agg = aggregate_instance(&conn, "i").unwrap();
        assert_eq!(agg.input_tokens, Some(100));
        assert_eq!(
            agg.cost_usd, None,
            "reported Unknown-cost stock is Unknown, not $0"
        );
        assert_eq!(agg.budget_status(Some(0.01)), BudgetStatus::Unknown);
        let sup = aggregate_supply(&conn, "p", "m").unwrap();
        assert_eq!(sup.cost_usd, None, "supply Unknown too");
        assert_eq!(sup.budget_status(Some(0.01)), BudgetStatus::Unknown);

        // Add a different session with a real $0.01 bill: aggregate stays Unknown.
        let r1 = r44_wire_record(
            "i",
            2,
            "session",
            Some("s1"),
            Some("s1"),
            None,
            1,
            "reported",
            [Basket::Unknown; 5],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:02.000Z"),
        );
        insert_journal_raw(&conn, &r1);
        let row = project_usage_event(&r1, Some("p"), Some("m")).unwrap();
        insert_usage_event(&conn, &row).unwrap();
        let agg = aggregate_instance(&conn, "i").unwrap();
        assert_eq!(
            agg.cost_usd, None,
            "one Unknown-cost stock poisons the $0.01 bill"
        );
        assert_eq!(agg.budget_status(Some(0.01)), BudgetStatus::Unknown);
    }

    #[test]
    fn r44_first_tokenless_turn_recovers_metadata_from_fallback() {
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        // First frame: tokenless (all baskets Unknown), $0.03 bill, 1M window,
        // native time. Written via the 14-column fallback path.
        let first = r44_wire_record(
            "i",
            1,
            "turn",
            Some("msg-T"),
            Some("sess-S"),
            Some("msg-T"),
            1,
            "estimated",
            [Basket::Unknown; 5],
            Some("0.03"),
            Some(1_000_000),
            Some("2026-10-20T00:00:00.000Z"),
        );
        insert_journal_raw(&conn, &first);
        let row = project_usage_event(&first, Some("p"), Some("m")).unwrap();
        // The fallback row stores only 14 columns: no scope/window/clock.
        insert_usage_event(&conn, &row).unwrap();
        // A later counter revision with no metadata (revision 2) — migration
        // must not let the first frame's metadata be lost or wrongly applied.
        migrate(&conn).unwrap();
        let once = r42_full_turn_row(&conn, "i");
        // columns: rev, profile, model, mode, scope_id, turn_id, scope, total,
        // input, output, cr, cw, cost, accounting, at, at_source, window,
        // lineage, sid
        assert_eq!(once[4], "msg-T", "scope id recovered");
        assert_eq!(once[18], "sess-S", "provenance recovered");
        assert_eq!(once[16], "1000000", "first frame window recovered");
        assert_eq!(once[15], "native", "first frame native clock recovered");
        assert_eq!(once[14], "2026-10-20T00:00:00.000Z");
        assert_eq!(once[12], "0.03");
        // A second migration changes nothing.
        migrate(&conn).unwrap();
        let twice = r42_full_turn_row(&conn, "i");
        assert_eq!(once, twice, "idempotent recovery");
    }

    fn r45_fallback_vs_live(
        records: &[JournalRecord],
        profile: &str,
        model: &str,
    ) -> (Connection, Connection) {
        let fb = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&fb);
        create_journal_table(&fb);
        for r in records {
            insert_journal_raw(&fb, r);
            let row = project_usage_event(r, Some(profile), Some(model)).unwrap();
            // Schema is not current -> the 14-column fallback insert path.
            insert_usage_event(&fb, &row).unwrap();
        }
        migrate(&fb).unwrap();
        migrate(&fb).unwrap();

        let lv = Connection::open_in_memory().unwrap();
        migrate(&lv).unwrap();
        create_journal_table(&lv);
        // AE3(d): every frame here is a PRE-CUTOVER producer frame (the cutover
        // on `fb` is MAX(journal seq) when lineage_state is introduced), so the
        // independent reference ingests them with legacy provenance — the same
        // era `fb` rebuilds — rather than post-cutover live.
        for r in records {
            insert_journal_raw(&lv, r);
            let row = project_usage_event(r, Some(profile), Some(model)).unwrap();
            insert_usage_event_era(&lv, &row, IngestEra::Legacy).unwrap();
        }
        (fb, lv)
    }

    #[test]
    fn r45_ae_a_tokenless_session_carrier_replays_equal_to_live() {
        let rec = r44_wire_record(
            "ins_ae_a",
            1,
            "session",
            Some("s1"),
            Some("s1"),
            None,
            1,
            "reported",
            [Basket::Unknown; 5],
            Some("0.01"),
            Some(1_000_000),
            Some("2026-10-20T00:00:00.000Z"),
        );
        let (fb, lv) = r45_fallback_vs_live(&[rec], "pvp", "m-a");
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "fallback replay equals independent live ingest"
        );
        let req = RollupRequest {
            instance_id: "ins_ae_a",
            kind: "codex",
            spec_model: Some("m-a"),
            effective_model: None,
            profile_id: Some("pvp"),
        };
        let rollup = rollup_instance(&fb, &req).unwrap().unwrap();
        assert_eq!(
            rollup.context_window_tokens,
            Some(1_000_000),
            "the tokenless carrier's reported 1M window resolves after replay"
        );
        let agg = aggregate_instance(&fb, "ins_ae_a").unwrap();
        assert_eq!(agg.cost_usd, Some(0.01));
    }

    #[test]
    fn r45_ae_b_envelope_only_first_turn_replays_equal_to_live() {
        // Build the record then strip the payload sourceSessionId explicitly
        // (a deliberate historical omission), keeping the envelope binding.
        let mut rec = r44_wire_record(
            "ins_ae_b",
            1,
            "turn",
            Some("msg-T"),
            Some("sess-S"),
            Some("msg-T"),
            1,
            "estimated",
            [Basket::Unknown; 5],
            Some("0.03"),
            Some(1_000_000),
            Some("2026-10-20T00:00:00.000Z"),
        );
        rec.event
            .get_mut("payload")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("sourceSessionId");
        let (fb, lv) = r45_fallback_vs_live(&[rec], "pvp", "m-a");
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "envelope-only identity replays equal to live"
        );
        let sid: Option<String> = fb
            .query_row(
                "SELECT source_session_id FROM usage_events WHERE instance_id='ins_ae_b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sid.as_deref(), Some("sess-S"), "envelope session recovered");
        let window: Option<i64> = fb
            .query_row(
                "SELECT native_context_window FROM usage_events WHERE instance_id='ins_ae_b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(window, Some(1_000_000));
    }

    #[test]
    fn r45_ae_c_stale_revision_replay_rejected_like_live() {
        let first = r44_wire_record(
            "ins_ae_c",
            1,
            "turn",
            Some("msg-T"),
            Some("sess-S"),
            Some("msg-T"),
            10,
            "estimated",
            [Basket::Unknown; 5],
            Some("0.03"),
            Some(1_000_000),
            Some("2026-10-20T00:00:00.000Z"),
        );
        let stale = r44_wire_record(
            "ins_ae_c",
            2,
            "turn",
            Some("msg-T"),
            Some("sess-S"),
            Some("msg-T"),
            1,
            "estimated",
            [Basket::Unknown; 5],
            None,
            None,
            None,
        );
        let (fb, lv) = r45_fallback_vs_live(&[first, stale], "pvp", "m-a");
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "stale-revision replay matches live rejection"
        );
        let (n, rev, window, source): (i64, i64, Option<i64>, String) = fb
            .query_row(
                "SELECT COUNT(*), MAX(metric_revision),
                        COALESCE(MAX(native_context_window), NULL),
                        COALESCE(MIN(observed_at_source), '')
                 FROM usage_events WHERE instance_id='ins_ae_c' AND scope='turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "stale rev1 replay rejected, one rev10 frame survives");
        assert_eq!(rev, 10);
        assert_eq!(window, Some(1_000_000), "rev10 metadata survives");
        assert_eq!(source, "native");
    }

    #[test]
    fn r45_ae_d_first_then_counter_then_second_migrate_equals_live() {
        // Fallback: first tokenless frame under r28, then migrate.
        let fb = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&fb);
        create_journal_table(&fb);
        let first = r44_wire_record(
            "ins_ae_d",
            1,
            "turn",
            Some("msg-T"),
            Some("sess-S"),
            Some("msg-T"),
            1,
            "estimated",
            [Basket::Unknown; 5],
            Some("0.03"),
            Some(1_000_000),
            Some("2026-10-20T00:00:00.000Z"),
        );
        insert_journal_raw(&fb, &first);
        insert_usage_event(
            &fb,
            &project_usage_event(&first, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        migrate(&fb).unwrap();

        // A counter-bearing revision 2 with NO window/nativeAt arrives AFTER
        // the upgrade (live path).
        let counter = r44_wire_record(
            "ins_ae_d",
            2,
            "turn",
            Some("msg-T"),
            Some("sess-S"),
            Some("msg-T"),
            2,
            "estimated",
            [
                Basket::Known(500_000),
                Basket::Known(500_000),
                Basket::NotApplicable,
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("0.03"),
            None,
            None,
        );
        insert_journal_raw(&fb, &counter);
        insert_usage_event(
            &fb,
            &project_usage_event(&counter, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        migrate(&fb).unwrap();

        // Independent era-aware reference: frame1 is a pre-cutover producer
        // frame (legacy); the post-upgrade counter frame2 is a higher revision
        // and is live, so accepting it moves the merged row's era to live
        // (AE3: provenance follows the accepted authoritative frame).
        let lv = Connection::open_in_memory().unwrap();
        migrate(&lv).unwrap();
        create_journal_table(&lv);
        for (r, era) in [(&first, IngestEra::Legacy), (&counter, IngestEra::Live)] {
            insert_journal_raw(&lv, r);
            insert_usage_event_era(
                &lv,
                &project_usage_event(r, Some("pvp"), Some("m-a")).unwrap(),
                era,
            )
            .unwrap();
        }
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "fallback -> live counter -> migrate equals era-aware reference"
        );
        let (window, total): (Option<i64>, Option<i64>) = fb
            .query_row(
                "SELECT native_context_window, total_tokens
                 FROM usage_events WHERE instance_id='ins_ae_d' AND scope='turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            window,
            Some(1_000_000),
            "counter frame retains first window"
        );
        assert_eq!(total, Some(500_000), "counter frame contributes counters");
    }

    // ---- r46 AE2: complete, undecorated instance replay --------------------

    /// An UPGRADE fixture: a previous-release database that already carries the
    /// full current schema (and the live invariant indexes) but NOT the
    /// one-time `journal_replay_v1` marker, so the reopen migration replays.
    fn r46_upgrade_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        conn.execute(
            "DELETE FROM usage_schema_meta WHERE key = 'journal_replay_v1'",
            [],
        )
        .unwrap();
        conn
    }

    /// Independent live reference: a fresh current-schema database ingesting
    /// the same journal frames through the live path in journal order.
    fn r46_live_reference(records: &[JournalRecord]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        for r in records {
            insert_journal_raw(&conn, r);
            let row = project_usage_event(r, Some("pvp"), Some("m-a")).unwrap();
            insert_usage_event(&conn, &row).unwrap();
        }
        conn
    }

    /// Instance aggregate, per-model supply and rollup must agree column-for-
    /// column with the independent live reference.
    fn r46_assert_accounting_equal(fb: &Connection, lv: &Connection, inst: &str) {
        let a = aggregate_instance(fb, inst).unwrap();
        let b = aggregate_instance(lv, inst).unwrap();
        assert_eq!(a.events, b.events, "{inst} events");
        assert_eq!(a.ambiguous_events, b.ambiguous_events, "{inst} ambiguous");
        assert_eq!(a.ambiguous, b.ambiguous, "{inst} ambiguous flag");
        assert_eq!(a.total_tokens, b.total_tokens, "{inst} total");
        assert_eq!(a.input_tokens, b.input_tokens, "{inst} input");
        assert_eq!(a.output_tokens, b.output_tokens, "{inst} output");
        assert_eq!(a.token_event_rows, b.token_event_rows, "{inst} rows");
        assert_eq!(a.unavailable, b.unavailable, "{inst} unavailable");
        match (a.cost_usd, b.cost_usd) {
            (Some(x), Some(y)) => assert!((x - y).abs() < 1e-9, "{inst} cost {x} vs {y}"),
            (x, y) => assert_eq!(x, y, "{inst} cost Unknown parity"),
        }
        let model = "m-a";
        let sa = aggregate_supply(fb, "pvp", model).unwrap();
        let sb = aggregate_supply(lv, "pvp", model).unwrap();
        assert_eq!(sa.events, sb.events, "{inst} supply events");
        assert_eq!(
            sa.ambiguous_events, sb.ambiguous_events,
            "{inst} supply ambiguous"
        );
        assert_eq!(sa.total_tokens, sb.total_tokens, "{inst} supply total");
        assert_eq!(sa.input_tokens, sb.input_tokens, "{inst} supply input");
        assert_eq!(sa.output_tokens, sb.output_tokens, "{inst} supply output");
        assert_eq!(sa.cost_usd, sb.cost_usd, "{inst} supply cost");
        let req = RollupRequest {
            instance_id: inst,
            kind: "codex",
            spec_model: Some("m-a"),
            effective_model: None,
            profile_id: Some("pvp"),
        };
        assert_eq!(
            rollup_instance(fb, &req).unwrap(),
            rollup_instance(lv, &req).unwrap(),
            "{inst} rollup"
        );
    }

    #[test]
    fn r46_a_upgrade_message_revision_chain_replays_the_full_journal() {
        // The r45 high: the live Message upsert keeps the FIRST seq while rev2
        // arrives at seq2, so selecting frames by surviving physical seq rolled
        // rev2 back to rev1.
        let rec1 = r44_wire_record(
            "ins_r46a",
            1,
            "message",
            Some("mk:file-X"),
            None,
            Some("mt1"),
            1,
            "estimated",
            [
                Basket::Unknown,
                Basket::Known(100),
                Basket::Known(0),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        let rec2 = r44_wire_record(
            "ins_r46a",
            2,
            "message",
            Some("mk:file-X"),
            None,
            Some("mt1"),
            2,
            "estimated",
            [
                Basket::Unknown,
                Basket::Known(150),
                Basket::Known(10),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("0.02"),
            None,
            Some("2026-10-20T00:00:02.000Z"),
        );
        let fb = r46_upgrade_fixture();
        for r in [&rec1, &rec2] {
            insert_journal_raw(&fb, r);
            let row = project_usage_event(r, Some("pvp"), Some("m-a")).unwrap();
            insert_usage_event(&fb, &row).unwrap();
        }
        // The live upsert collapsed both frames onto one seq-1 physical row.
        let n_before: i64 = fb
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id='ins_r46a' AND scope='message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_before, 1);
        migrate(&fb).unwrap();
        let (rev, inp, cost): (i64, Option<i64>, Option<String>) = fb
            .query_row(
                "SELECT metric_revision, input_tokens, cost_usd FROM usage_events
                 WHERE instance_id='ins_r46a' AND scope='message'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(rev, 2, "accepted revision 2 survives replay");
        assert_eq!(inp, Some(150), "accepted counters survive replay");
        assert_eq!(cost.as_deref(), Some("0.02"), "corrected bill survives");
        let lv = r46_live_reference(&[rec1, rec2]);
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "full rows equal the independent live reference"
        );
        r46_assert_accounting_equal(&fb, &lv, "ins_r46a");
        let once = r43_dump_rows(&fb);
        migrate(&fb).unwrap();
        assert_eq!(once, r43_dump_rows(&fb), "migrate twice == once");
    }

    #[test]
    fn r46_b_upgrade_turn_final_tokenless_correction() {
        // Complementary Turn failure: the live Turn upsert ADVANCES seq onto
        // the tokenless correction's seq; replaying only that seq loses the
        // counters/window/native time of the first counter frame.
        let rec1 = r44_wire_record(
            "ins_r46b",
            1,
            "turn",
            Some("t1"),
            Some("sess-S"),
            Some("t1"),
            1,
            "estimated",
            [
                Basket::Known(110),
                Basket::Known(100),
                Basket::Known(10),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some("0.01"),
            Some(1_000_000),
            Some("2026-10-20T00:00:01.000Z"),
        );
        let rec2 = r44_wire_record(
            "ins_r46b",
            2,
            "turn",
            Some("t1"),
            Some("sess-S"),
            Some("t1"),
            2,
            "estimated",
            [Basket::Unknown; 5],
            Some("0.02"),
            None,
            None,
        );
        let fb = r46_upgrade_fixture();
        for r in [&rec1, &rec2] {
            insert_journal_raw(&fb, r);
            let row = project_usage_event(r, Some("pvp"), Some("m-a")).unwrap();
            insert_usage_event(&fb, &row).unwrap();
        }
        let (phys_seq,): (i64,) = fb
            .query_row(
                "SELECT seq FROM usage_events
                 WHERE instance_id='ins_r46b' AND scope='turn'",
                [],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(phys_seq, 2, "the live turn upsert advanced seq to 2");
        migrate(&fb).unwrap();
        struct TurnAfter {
            revision: i64,
            seq: Option<i64>,
            total: Option<i64>,
            input: Option<i64>,
            output: Option<i64>,
            window: Option<i64>,
            at_source: String,
        }
        let row: TurnAfter = fb
            .query_row(
                "SELECT metric_revision, seq, total_tokens, input_tokens, output_tokens,
                        native_context_window, observed_at_source
                 FROM usage_events WHERE instance_id='ins_r46b' AND scope='turn'",
                [],
                |r| {
                    Ok(TurnAfter {
                        revision: r.get(0)?,
                        seq: r.get(1)?,
                        total: r.get(2)?,
                        input: r.get(3)?,
                        output: r.get(4)?,
                        window: r.get(5)?,
                        at_source: r.get(6)?,
                    })
                },
            )
            .unwrap();
        assert_eq!(row.revision, 2);
        assert_eq!(
            row.seq,
            Some(2),
            "surviving row stays at the correction seq"
        );
        assert_eq!(row.total, Some(110), "first-frame counters retained");
        assert_eq!(row.input, Some(100));
        assert_eq!(row.output, Some(10));
        assert_eq!(row.window, Some(1_000_000), "first-frame window retained");
        let lv = r46_live_reference(&[rec1.clone(), rec2.clone()]);
        let cost: Option<String> = fb
            .query_row(
                "SELECT cost_usd FROM usage_events
                 WHERE instance_id='ins_r46b' AND scope='turn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cost.as_deref(), Some("0.02"), "the correction bill wins");
        assert_eq!(row.at_source, "native", "first-frame native clock retained");
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "full rows equal the independent live reference"
        );
        r46_assert_accounting_equal(&fb, &lv, "ins_r46b");
        let once = r43_dump_rows(&fb);
        migrate(&fb).unwrap();
        assert_eq!(once, r43_dump_rows(&fb), "migrate twice == once");
    }

    #[test]
    fn r47_i_pre_cutover_unmatched_turn_beside_scoped_session_is_conservative_unknown() {
        // Ruling AE3, codex r46 medium-1: lineage_state is PRODUCER-ERA
        // provenance, not a recovery tag. Upgrading a schema WITHOUT the
        // lineage column, an identified Turn (known counters, no proven source
        // session) next to a scoped Session is a PRE-CUTOVER producer frame. It
        // ends 'legacy' even though it is unjournaled (the column-introduction
        // rule tags every row that exists at the cutover), and the documented
        // r29 conservative rule classifies it as ambiguous -> Unknown. The
        // current codex adapter emits the same shape but is post-cutover live;
        // producer era is the only discriminator.
        let turn = r44_wire_record(
            "ins_r47i",
            1,
            "turn",
            Some("t1"),
            None,
            Some("t1"),
            1,
            "estimated",
            [
                Basket::Known(110),
                Basket::Known(100),
                Basket::Known(10),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        let session = r44_wire_record(
            "ins_r47i",
            2,
            "session",
            Some("s1"),
            Some("s1"),
            None,
            1,
            "estimated",
            [
                Basket::Known(280),
                Basket::Known(250),
                Basket::Known(30),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("3.00"),
            None,
            Some("2026-10-20T00:00:02.000Z"),
        );
        // Pre-lineage schema: every current column EXCEPT lineage_state. Build
        // a current DB then drop the column and the era/marker state, so the
        // reopen migration re-introduces it and applies the cutover rule.
        let fb = Connection::open_in_memory().unwrap();
        migrate(&fb).unwrap();
        create_journal_table(&fb);
        fb.execute_batch(
            "ALTER TABLE usage_events DROP COLUMN lineage_state;
             DELETE FROM usage_schema_meta;
             DELETE FROM usage_instance_era;",
        )
        .unwrap();
        // The identified Turn is UNJOURNALED but retains its scope columns.
        let turn_row = project_usage_event(&turn, Some("pvp"), Some("m-a")).unwrap();
        r47_raw_insert_no_lineage(&fb, &turn_row);
        // The scoped Session IS journal-backed: replay rebuilds it scoped.
        insert_journal_raw(&fb, &session);
        let session_row = project_usage_event(&session, Some("pvp"), Some("m-a")).unwrap();
        r47_raw_insert_no_lineage(&fb, &session_row);
        migrate(&fb).unwrap();

        // Both rows carry the producer-era tag.
        let states: Vec<String> = fb
            .prepare(
                "SELECT COALESCE(lineage_state,'') FROM usage_events
                 WHERE instance_id='ins_r47i' ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(states, vec!["legacy".to_string(), "legacy".to_string()]);

        // Conservative accounting: the unproven legacy turn beside scoped
        // stock is ambiguous and poisons to Unknown.
        let agg = aggregate_instance(&fb, "ins_r47i").unwrap();
        assert_eq!(agg.ambiguous_events, 1, "the pre-cutover turn is ambiguous");
        assert!(agg.ambiguous);
        assert_eq!(
            agg.input_tokens, None,
            "known counters are conservative Unknown"
        );
        assert_eq!(agg.cost_usd, None, "the bill is Unknown, not $0.01");
        let supply = aggregate_supply(&fb, "pvp", "m-a").unwrap();
        assert_eq!(supply.ambiguous_events, 1);
        assert_eq!(supply.cost_usd, None);

        // Idempotent.
        let once = r43_dump_rows(&fb);
        migrate(&fb).unwrap();
        assert_eq!(once, r43_dump_rows(&fb), "migrate twice == once");
    }

    #[test]
    fn r47_post_cutover_unmatched_turn_beside_scoped_session_bills_ruling_b_flow() {
        // AE3 counterpart: the SAME shape produced AFTER the cutover (column
        // present, live ingest) is live (NULL) and bills as unmatched Ruling B
        // flow with no ambiguity — the discriminator is producer era.
        let turn = r44_wire_record(
            "ins_r47live",
            1,
            "turn",
            Some("t1"),
            None,
            Some("t1"),
            1,
            "estimated",
            [
                Basket::Known(110),
                Basket::Known(100),
                Basket::Known(10),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        let session = r44_wire_record(
            "ins_r47live",
            2,
            "session",
            Some("s1"),
            Some("s1"),
            None,
            1,
            "estimated",
            [
                Basket::Known(280),
                Basket::Known(250),
                Basket::Known(30),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("3.00"),
            None,
            Some("2026-10-20T00:00:02.000Z"),
        );
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        for r in [&turn, &session] {
            insert_journal_raw(&conn, r);
            let row = project_usage_event(r, Some("pvp"), Some("m-a")).unwrap();
            insert_usage_event(&conn, &row).unwrap();
        }
        let states: Vec<Option<String>> = conn
            .prepare(
                "SELECT lineage_state FROM usage_events
                 WHERE instance_id='ins_r47live' ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(states, vec![None, None], "post-cutover rows stay live/NULL");
        let agg = aggregate_instance(&conn, "ins_r47live").unwrap();
        assert_eq!(
            agg.ambiguous_events, 0,
            "a live identified turn is not ambiguous"
        );
        assert_eq!(agg.input_tokens, Some(100), "the unmatched live turn bills");
        assert_eq!(agg.cost_usd, Some(0.01));
    }

    /// Build one identified turn + one scoped session UsageEventRow pair.
    fn r47_turn_and_session(
        inst: &str,
        tseq: i64,
        sseq: i64,
    ) -> (UsageEventRow, JournalRecord, JournalRecord) {
        let turn = r44_wire_record(
            inst,
            tseq,
            "turn",
            Some("t1"),
            None,
            Some("t1"),
            1,
            "estimated",
            [
                Basket::Known(110),
                Basket::Known(100),
                Basket::Known(10),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        let session = r44_wire_record(
            inst,
            sseq,
            "session",
            Some("s1"),
            Some("s1"),
            None,
            1,
            "estimated",
            [
                Basket::Known(280),
                Basket::Known(250),
                Basket::Known(30),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("3.00"),
            None,
            Some("2026-10-20T00:00:02.000Z"),
        );
        let turn_row = project_usage_event(&turn, Some("pvp"), Some("m-a")).unwrap();
        (turn_row, turn, session)
    }

    #[test]
    fn r47_ii_column_present_marker_absent_keeps_live_rows_null_and_billing() {
        // AE3(c): the column already exists (unreleased intermediate branch
        // build) but no cutover was recorded. Instance A holds legacy-tagged
        // rows; instance B holds post-cutover NULL rows. Reopen must derive the
        // cutover PER INSTANCE (A: MAX seq among legacy; B: 0), replay with the
        // right era, and never blanket-retag B's live rows. This is codex r45's
        // "blanket update reaches live rows" case.
        let (a_turn_row, a_turn, a_session) = r47_turn_and_session("ins_r47A", 1, 2);
        let (b_turn_row, b_turn, b_session) = r47_turn_and_session("ins_r47B", 1, 2);
        let a_session_row = project_usage_event(&a_session, Some("pvp"), Some("m-a")).unwrap();
        let b_session_row = project_usage_event(&b_session, Some("pvp"), Some("m-a")).unwrap();

        let conn = r46_upgrade_fixture(); // column present, replay marker removed
        // Instance A: rows explicitly tagged legacy (pre-cutover producer).
        r46_raw_insert(&conn, &a_turn_row, Some("legacy"));
        r46_raw_insert(&conn, &a_session_row, Some("legacy"));
        // Instance B: post-cutover live rows, lineage NULL.
        r46_raw_insert(&conn, &b_turn_row, None);
        r46_raw_insert(&conn, &b_session_row, None);
        // Journal every frame so replay rebuilds them.
        for r in [&a_turn, &a_session, &b_turn, &b_session] {
            insert_journal_raw(&conn, r);
        }
        migrate(&conn).unwrap();

        // A stays legacy (conservative Unknown beside scoped stock).
        let a_states: Vec<String> = conn
            .prepare(
                "SELECT COALESCE(lineage_state,'') FROM usage_events
                 WHERE instance_id='ins_r47A' ORDER BY scope",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(a_states.iter().all(|s| s == "legacy"), "A: {a_states:?}");
        let a = aggregate_instance(&conn, "ins_r47A").unwrap();
        assert_eq!(a.ambiguous_events, 1, "A's legacy turn stays ambiguous");
        assert_eq!(a.input_tokens, None, "A stays conservative Unknown");

        // B stays live NULL and bills Ruling B flow.
        let b_states: Vec<Option<String>> = conn
            .prepare(
                "SELECT lineage_state FROM usage_events
                 WHERE instance_id='ins_r47B' ORDER BY scope",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            b_states.iter().all(std::option::Option::is_none),
            "B: {b_states:?}"
        );
        let b = aggregate_instance(&conn, "ins_r47B").unwrap();
        assert_eq!(b.ambiguous_events, 0, "B's live turn is not ambiguous");
        assert_eq!(
            b.input_tokens,
            Some(100),
            "B's live turn bills Ruling B flow"
        );
        assert_eq!(b.cost_usd, Some(0.01));

        // Idempotent.
        let once = r43_dump_rows(&conn);
        migrate(&conn).unwrap();
        assert_eq!(once, r43_dump_rows(&conn), "migrate twice == once");
    }

    #[test]
    fn r47_iii_one_instance_pre_and_post_cutover_frames_get_per_row_era() {
        // AE3(b): one instance straddles the cutover. seq1 is a pre-cutover
        // turn (legacy); a post-upgrade seq2 frame is live. Forced replay must
        // assign each its era, equal to an independent era-aware reference.
        let first = r44_wire_record(
            "ins_r47iii",
            1,
            "turn",
            Some("t1"),
            None,
            Some("t1"),
            1,
            "estimated",
            [
                Basket::Known(110),
                Basket::Known(100),
                Basket::Known(10),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        // Fallback first frame under a pre-lineage schema.
        let fb = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&fb);
        create_journal_table(&fb);
        insert_journal_raw(&fb, &first);
        insert_usage_event(
            &fb,
            &project_usage_event(&first, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        migrate(&fb).unwrap(); // column introduced; cutover = MAX(seq) = 1.

        // Post-cutover seq2 frame, a live identified turn (distinct scope).
        let second = r44_wire_record(
            "ins_r47iii",
            2,
            "turn",
            Some("t2"),
            None,
            Some("t2"),
            1,
            "estimated",
            [
                Basket::Known(220),
                Basket::Known(200),
                Basket::Known(20),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some("0.02"),
            None,
            Some("2026-10-20T00:00:03.000Z"),
        );
        insert_journal_raw(&fb, &second);
        insert_usage_event(
            &fb,
            &project_usage_event(&second, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        // Force the complete replay over the collapsed/era-mixed rows.
        fb.execute(
            "DELETE FROM usage_schema_meta WHERE key = 'journal_replay_v1'",
            [],
        )
        .unwrap();
        migrate(&fb).unwrap();

        let eras: Vec<(Option<i64>, Option<String>)> = fb
            .prepare(
                "SELECT seq, lineage_state FROM usage_events
                 WHERE instance_id='ins_r47iii' AND scope='turn' ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            eras,
            vec![(Some(1), Some("legacy".into())), (Some(2), None)],
            "seq1 pre-cutover is legacy; seq2 post-cutover is live"
        );

        // Independent era-aware reference.
        let lv = Connection::open_in_memory().unwrap();
        migrate(&lv).unwrap();
        create_journal_table(&lv);
        for (r, era) in [(&first, IngestEra::Legacy), (&second, IngestEra::Live)] {
            insert_journal_raw(&lv, r);
            insert_usage_event_era(
                &lv,
                &project_usage_event(r, Some("pvp"), Some("m-a")).unwrap(),
                era,
            )
            .unwrap();
        }
        assert_eq!(r43_dump_rows(&fb), r43_dump_rows(&lv));
        r46_assert_accounting_equal(&fb, &lv, "ins_r47iii");
        let once = r43_dump_rows(&fb);
        migrate(&fb).unwrap();
        assert_eq!(once, r43_dump_rows(&fb), "migrate twice == once");
    }

    #[test]
    fn r47_fold_message_collision_ignores_source_session_id() {
        // AE3 item 2 (codex r46 medium-2): the Message unique key is
        // (instance, scope, scope_id) — source_session_id is NOT part of it.
        // Two unjournaled Messages with the same durable key but different
        // source sessions (rev1/A, rev2/B) must fold to ONE rev2 row; grouping
        // that kept a survivor per session would leave a collision and abort
        // the whole migration at CREATE UNIQUE INDEX.
        let base = UsageEventRow {
            instance_id: "ins_r47fold".into(),
            seq: 1,
            profile_id: Some("pvp".into()),
            model: Some("m-a".into()),
            scope: "message".into(),
            scope_id: Some("mk1".into()),
            turn_id: None,
            metric_revision: 1,
            source_session_id: Some("sess-A".into()),
            mode: "snapshot".into(),
            total_tokens: Some(100),
            input_tokens: Some(100),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("0.01".into()),
            accounting: "estimated".into(),
            observed_at: "2026-10-20T00:00:01.000Z".into(),
            observed_at_source: "native".into(),
            native_context_window: None,
        };
        let mut rev2 = base.clone();
        rev2.seq = 2;
        rev2.metric_revision = 2;
        rev2.source_session_id = Some("sess-B".into());
        rev2.total_tokens = Some(160);
        rev2.input_tokens = Some(150);
        rev2.output_tokens = Some(10);
        rev2.cost_usd = Some("0.02".into());
        rev2.observed_at = "2026-10-20T00:00:02.000Z".into();

        // Column-present marker-absent DB; drop the message index so both
        // physical rows can coexist (rolled-back window). No journal rows, so
        // neither is backed — the fold is the only thing that touches them.
        let conn = r46_upgrade_fixture();
        conn.execute_batch("DROP INDEX IF EXISTS usage_events_message_dedupe;")
            .unwrap();
        r46_raw_insert(&conn, &base, None);
        r46_raw_insert(&conn, &rev2, None);
        migrate(&conn).unwrap(); // must commit, not fail on the unique index

        let (n, rev, inp, cost): (i64, i64, Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(metric_revision), MAX(input_tokens),
                        COALESCE(MAX(cost_usd), NULL)
                 FROM usage_events
                 WHERE instance_id='ins_r47fold' AND scope='message'
                   AND scope_id='mk1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "the two sessions collapse onto the one message key");
        assert_eq!(rev, 2, "higher revision wins");
        assert_eq!(inp, Some(150), "rev2 counters are folded, not discarded");
        assert_eq!(cost.as_deref(), Some("0.02"));
        // The message index exists and is unique.
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='usage_events_message_dedupe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
        let once = r43_dump_rows(&conn);
        migrate(&conn).unwrap();
        assert_eq!(once, r43_dump_rows(&conn), "migrate twice == once");
    }

    // ===== r48: era follows the accepted authoritative frame =====

    /// Build an identified Turn row (scope_id t1, NO proven source session) for
    /// the given revision/counters/cost, plus a scoped Session row beside it.
    fn r48_turn(
        inst: &str,
        seq: i64,
        rev: i64,
        input: i64,
        cost: &str,
    ) -> (JournalRecord, UsageEventRow) {
        let rec = r44_wire_record(
            inst,
            seq,
            "turn",
            Some("t1"),
            None,
            Some("t1"),
            rev as u64,
            "estimated",
            [
                Basket::Known(input + 10),
                Basket::Known(input),
                Basket::Known(10),
                Basket::Known(0),
                Basket::Known(0),
            ],
            Some(cost),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        let row = project_usage_event(&rec, Some("pvp"), Some("m-a")).unwrap();
        (rec, row)
    }

    /// Add the scoped Session that makes a legacy unproven turn ambiguous but
    /// lets a live unproven turn bill Ruling B flow.
    fn r48_add_scoped_session(conn: &Connection, inst: &str) {
        let rec = r44_wire_record(
            inst,
            9,
            "session",
            Some("s1"),
            Some("s1"),
            None,
            1,
            "estimated",
            [
                Basket::Known(500),
                Basket::Known(500),
                Basket::NotApplicable,
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("9.00"),
            None,
            Some("2026-10-20T00:00:09.000Z"),
        );
        insert_usage_event(
            conn,
            &project_usage_event(&rec, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
    }

    fn r48_turn_era(conn: &Connection, inst: &str) -> Option<String> {
        conn.query_row(
            "SELECT lineage_state FROM usage_events
             WHERE instance_id = ?1 AND scope='turn' AND scope_id='t1'",
            params![inst],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn r48_turn_legacy_rev1_accepted_live_rev2_becomes_live_and_bills_flow() {
        // AE3 clarification: a higher accepted revision carries ITS era.
        // Legacy rev1 turn beside scoped stock is ambiguous; once a live rev2
        // is accepted for the same key the row goes NULL and bills Ruling B.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        let (_, rev1) = r48_turn("ins_r48tl", 1, 1, 100, "0.01");
        insert_usage_event_era(&conn, &rev1, IngestEra::Legacy).unwrap();
        r48_add_scoped_session(&conn, "ins_r48tl");
        assert_eq!(r48_turn_era(&conn, "ins_r48tl").as_deref(), Some("legacy"));
        let before = aggregate_instance(&conn, "ins_r48tl").unwrap();
        assert_eq!(before.ambiguous_events, 1, "legacy rev1 is ambiguous");

        let (_, rev2) = r48_turn("ins_r48tl", 2, 2, 110, "0.02");
        let accepted = insert_usage_event(&conn, &rev2).unwrap(); // live
        assert!(accepted, "the higher live revision is accepted");
        assert_eq!(
            r48_turn_era(&conn, "ins_r48tl"),
            None,
            "row era follows live rev2"
        );
        let after = aggregate_instance(&conn, "ins_r48tl").unwrap();
        assert_eq!(
            after.ambiguous_events, 0,
            "live turn is no longer ambiguous"
        );
        assert_eq!(
            after.input_tokens,
            Some(110),
            "live rev2 bills Ruling B flow"
        );
        assert_eq!(after.cost_usd, Some(0.02));
    }

    #[test]
    fn r48_turn_live_rev1_then_replayed_legacy_rev2_becomes_legacy() {
        // A pre-cutover (legacy) frame can carry a higher revision for a key
        // whose current row is live; accepting it makes the row legacy.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        let (_, rev1) = r48_turn("ins_r48ll", 1, 1, 100, "0.01");
        insert_usage_event(&conn, &rev1).unwrap();
        r48_add_scoped_session(&conn, "ins_r48ll");
        assert_eq!(r48_turn_era(&conn, "ins_r48ll"), None);

        let (_, rev2) = r48_turn("ins_r48ll", 2, 2, 110, "0.02");
        let accepted = insert_usage_event_era(&conn, &rev2, IngestEra::Legacy).unwrap();
        assert!(accepted);
        assert_eq!(
            r48_turn_era(&conn, "ins_r48ll").as_deref(),
            Some("legacy"),
            "row era follows the accepted pre-cutover rev2"
        );
        let agg = aggregate_instance(&conn, "ins_r48ll").unwrap();
        assert_eq!(
            agg.ambiguous_events, 1,
            "now a legacy turn beside stock -> ambiguous"
        );
        assert_eq!(agg.input_tokens, None, "conservative Unknown");
        assert_eq!(agg.cost_usd, None);
    }

    #[test]
    fn r48_turn_lower_live_revision_rejected_keeps_legacy_era() {
        // A lower live revision must not overwrite a stored legacy rev2.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        let (_, rev2) = r48_turn("ins_r48rj", 2, 2, 110, "0.02");
        insert_usage_event_era(&conn, &rev2, IngestEra::Legacy).unwrap();
        let (_, rev1) = r48_turn("ins_r48rj", 1, 1, 100, "0.01");
        let accepted = insert_usage_event(&conn, &rev1).unwrap();
        assert!(!accepted, "the lower revision is rejected");
        assert_eq!(
            r48_turn_era(&conn, "ins_r48rj").as_deref(),
            Some("legacy"),
            "rejected revision leaves stored provenance untouched"
        );
        let (rev, inp): (i64, Option<i64>) = conn
            .query_row(
                "SELECT metric_revision, input_tokens FROM usage_events
                 WHERE instance_id='ins_r48rj' AND scope='turn' AND scope_id='t1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rev, inp), (2, Some(110)), "rev2 content stays");
    }

    /// Build a durable Message row at a given seq/revision/counters.
    fn r48_message(
        inst: &str,
        seq: i64,
        rev: i64,
        input: i64,
        cost: &str,
        sid: Option<&str>,
    ) -> UsageEventRow {
        let rec = r44_wire_record(
            inst,
            seq,
            "message",
            Some("mk1"),
            sid,
            Some("t1"),
            rev as u64,
            "estimated",
            [
                Basket::Unknown,
                Basket::Known(input),
                Basket::Known(5),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some(cost),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        project_usage_event(&rec, Some("pvp"), Some("m-a")).unwrap()
    }

    fn r48_message_era(conn: &Connection, inst: &str) -> Option<String> {
        conn.query_row(
            "SELECT lineage_state FROM usage_events
             WHERE instance_id = ?1 AND scope='message' AND scope_id='mk1'",
            params![inst],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn r48_message_accepted_revision_carries_its_era_rejected_keeps_stored() {
        // legacy rev1 -> live rev2 accepted => NULL; counters follow rev2.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        insert_usage_event_era(
            &conn,
            &r48_message("ins_m1", 1, 1, 100, "0.01", None),
            IngestEra::Legacy,
        )
        .unwrap();
        assert_eq!(r48_message_era(&conn, "ins_m1").as_deref(), Some("legacy"));
        assert!(
            insert_usage_event(&conn, &r48_message("ins_m1", 2, 2, 150, "0.02", None)).unwrap()
        );
        assert_eq!(r48_message_era(&conn, "ins_m1"), None, "live rev2 era wins");
        let inp: Option<i64> = conn
            .query_row(
                "SELECT input_tokens FROM usage_events
                 WHERE instance_id='ins_m1' AND scope='message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inp, Some(150));

        // live rev2 -> legacy rev3 accepted => legacy.
        assert!(
            insert_usage_event_era(
                &conn,
                &r48_message("ins_m1", 3, 3, 160, "0.03", None),
                IngestEra::Legacy
            )
            .unwrap()
        );
        assert_eq!(r48_message_era(&conn, "ins_m1").as_deref(), Some("legacy"));

        // lower live rev1 rejected => stays legacy.
        assert!(
            !insert_usage_event(&conn, &r48_message("ins_m1", 1, 1, 100, "0.01", None)).unwrap()
        );
        assert_eq!(r48_message_era(&conn, "ins_m1").as_deref(), Some("legacy"));
    }

    /// Two physical unjournaled message rows on one key, dropped index, then a
    /// real migrate folds them. The captured (later-rowid) row carries
    /// `captured_era` at revision `captured_rev`; returns the post-fold era.
    fn r48_fold_eras(
        survivor_era: Option<&str>,
        captured_era: Option<&str>,
        captured_rev: i64,
    ) -> Option<String> {
        let conn = r46_upgrade_fixture();
        conn.execute_batch("DROP INDEX IF EXISTS usage_events_message_dedupe;")
            .unwrap();
        let mut s1 = r48_message("ins_r48foldera", 1, 1, 100, "0.01", Some("sA"));
        s1.source_session_id = Some("sA".into());
        let mut s2 = r48_message("ins_r48foldera", 2, captured_rev, 150, "0.02", Some("sB"));
        s2.source_session_id = Some("sB".into());
        r46_raw_insert(&conn, &s1, survivor_era);
        r46_raw_insert(&conn, &s2, captured_era);
        migrate(&conn).unwrap();
        r48_message_era(&conn, "ins_r48foldera")
    }

    #[test]
    fn r48_mixed_era_fold_takes_winning_revisions_era() {
        // Captured legacy rev2 is higher -> accepted -> legacy.
        assert_eq!(
            r48_fold_eras(None, Some("legacy"), 2).as_deref(),
            Some("legacy"),
            "legacy revision wins the fold"
        );
        // Captured live rev2 is higher -> accepted -> NULL.
        assert_eq!(
            r48_fold_eras(Some("legacy"), None, 2),
            None,
            "live revision wins the fold"
        );
        // Captured live rev1 is LOWER than survivor legacy rev2 -> rejected,
        // survivor era (legacy) is retained.
        let conn = r46_upgrade_fixture();
        conn.execute_batch("DROP INDEX IF EXISTS usage_events_message_dedupe;")
            .unwrap();
        let mut survivor = r48_message("ins_r48fe", 1, 2, 150, "0.02", Some("sA"));
        survivor.source_session_id = Some("sA".into());
        let mut lower = r48_message("ins_r48fe", 2, 1, 100, "0.01", Some("sB"));
        lower.source_session_id = Some("sB".into());
        r46_raw_insert(&conn, &survivor, Some("legacy"));
        r46_raw_insert(&conn, &lower, None);
        migrate(&conn).unwrap();
        let era: Option<String> = conn
            .query_row(
                "SELECT lineage_state FROM usage_events
                 WHERE instance_id='ins_r48fe' AND scope='message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            era.as_deref(),
            Some("legacy"),
            "rejected captured rev keeps survivor era"
        );
    }

    #[test]
    fn r48_item2_column_intro_unjournaled_instance_without_journal_gets_cutover_0() {
        // Item 2: introduce lineage_state with an unjournaled usage row at
        // seq 10 in an instance that has NO journal frames. The cutover must be
        // explicitly recorded as 0 (not derived from the usage seq) and the row
        // is still legacy.
        let conn = Connection::open_in_memory().unwrap();
        // Pre-lineage current-schema-minus-lineage database.
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        conn.execute_batch(
            "ALTER TABLE usage_events DROP COLUMN lineage_state;
             DELETE FROM usage_schema_meta;
             DELETE FROM usage_instance_era;",
        )
        .unwrap();
        let mut row = r48_message("ins_r48nojourn", 10, 1, 100, "0.01", None);
        row.instance_id = "ins_r48nojourn".into();
        row.seq = 10;
        r47_raw_insert_no_lineage(&conn, &row); // deliberately NOT journaled

        migrate(&conn).unwrap();
        let cutover: i64 = conn
            .query_row(
                "SELECT cutover_seq FROM usage_instance_era WHERE instance_id='ins_r48nojourn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cutover, 0,
            "instance with no journal frames records boundary 0"
        );
        assert_eq!(
            r48_message_era(&conn, "ins_r48nojourn").as_deref(),
            Some("legacy"),
            "the row is still legacy (existed at cutover)"
        );
    }

    #[test]
    fn r48_item3_marker_applied_without_era_metadata_still_initializes() {
        // Item 3: lineage_state present and journal_replay_v1 already applied,
        // but usage_instance_era is empty. Migrate must still derive cutovers
        // (case c) before the completion guard returns.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        // A legacy row (intermediate build) and a live row in different
        // instances, then clear the era metadata and keep the replay marker.
        let legacy = r48_message("ins_r48mlegacy", 3, 1, 100, "0.01", None);
        let live = r48_message("ins_r48mlive", 7, 1, 100, "0.01", None);
        r46_raw_insert(&conn, &legacy, Some("legacy"));
        r46_raw_insert(&conn, &live, None);
        conn.execute("DELETE FROM usage_instance_era", []).unwrap();
        let marker: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_schema_meta WHERE key='journal_replay_v1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, 1, "fixture keeps the applied replay marker");

        migrate(&conn).unwrap();
        let legacy_cut: i64 = conn
            .query_row(
                "SELECT cutover_seq FROM usage_instance_era WHERE instance_id='ins_r48mlegacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let live_cut: i64 = conn
            .query_row(
                "SELECT cutover_seq FROM usage_instance_era WHERE instance_id='ins_r48mlive'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_cut, 3, "legacy row's seq is the derived boundary");
        assert_eq!(live_cut, 0, "all-live instance derives boundary 0");
        // Rows keep their era.
        assert_eq!(
            r48_message_era(&conn, "ins_r48mlegacy").as_deref(),
            Some("legacy")
        );
        assert_eq!(r48_message_era(&conn, "ins_r48mlive"), None);
        let once = r43_dump_rows(&conn);
        migrate(&conn).unwrap();
        assert_eq!(once, r43_dump_rows(&conn), "migrate twice == once");
    }

    /// Raw insert into a PRE-lineage schema (all current columns except
    /// lineage_state), bypassing the live upsert path.
    fn r47_raw_insert_no_lineage(conn: &Connection, row: &UsageEventRow) {
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens, cost_usd,
                 accounting, observed_at, observed_at_source, native_context_window)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            params![
                row.instance_id,
                row.seq,
                row.profile_id,
                row.model,
                row.scope,
                row.scope_id,
                row.turn_id,
                row.mode,
                row.metric_revision,
                row.source_session_id,
                row.total_tokens,
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.cost_usd,
                row.accounting,
                row.observed_at,
                row.observed_at_source,
                row.native_context_window,
            ],
        )
        .unwrap();
    }

    /// Raw insert of a usage row in the current-schema column set, bypassing
    /// the unique indexes (used to simulate rows accumulated while an earlier
    /// migration window was rolled back).
    fn r46_raw_insert(conn: &Connection, row: &UsageEventRow, lineage: Option<&str>) {
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens, cost_usd,
                 accounting, observed_at, observed_at_source, native_context_window,
                 lineage_state)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                row.instance_id,
                row.seq,
                row.profile_id,
                row.model,
                row.scope,
                row.scope_id,
                row.turn_id,
                row.mode,
                row.metric_revision,
                row.source_session_id,
                row.total_tokens,
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.cost_usd,
                row.accounting,
                row.observed_at,
                row.observed_at_source,
                row.native_context_window,
                lineage,
            ],
        )
        .unwrap();
    }

    #[test]
    fn r46_d_unjournaled_duplicates_fold_by_the_live_rule_else_unchanged() {
        // Defect 3: two unjournaled duplicate rows colliding on a new unique
        // key are folded in rowid order through the live upsert rule (higher
        // revision wins, omitted metadata retained) — never deleted by a
        // keep-MIN(rowid) sweep. Every other unjournaled row is byte-identical.
        let mk1_seq1 = UsageEventRow {
            instance_id: "ins_r46d".into(),
            seq: 1,
            profile_id: Some("pvp".into()),
            model: Some("m-a".into()),
            scope: "message".into(),
            scope_id: Some("mk1".into()),
            turn_id: None,
            metric_revision: 1,
            source_session_id: None,
            mode: "snapshot".into(),
            total_tokens: Some(100),
            input_tokens: Some(100),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("0.01".into()),
            accounting: "estimated".into(),
            observed_at: "2026-10-20T00:00:01.000Z".into(),
            observed_at_source: "native".into(),
            native_context_window: None,
        };
        let mut mk1_seq2 = mk1_seq1.clone();
        mk1_seq2.seq = 2;
        mk1_seq2.metric_revision = 2;
        mk1_seq2.total_tokens = Some(150);
        mk1_seq2.input_tokens = Some(150);
        mk1_seq2.output_tokens = Some(10);
        mk1_seq2.cost_usd = Some("0.02".into());
        mk1_seq2.observed_at = "2026-10-20T00:00:02.000Z".into();
        let t1_seq10 = UsageEventRow {
            seq: 10,
            scope: "turn".into(),
            scope_id: Some("t1".into()),
            metric_revision: 1,
            total_tokens: Some(100),
            input_tokens: Some(90),
            output_tokens: Some(10),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("1.00".into()),
            observed_at: "2026-10-20T00:00:03.000Z".into(),
            ..mk1_seq1.clone()
        };
        let mut t1_seq11 = t1_seq10.clone();
        t1_seq11.seq = 11;
        t1_seq11.metric_revision = 2;
        t1_seq11.total_tokens = Some(120);
        t1_seq11.input_tokens = Some(110);
        let mut mk2 = mk1_seq1.clone();
        mk2.seq = 20;
        mk2.scope_id = Some("mk2".into());
        mk2.total_tokens = Some(7);
        mk2.input_tokens = Some(7);
        mk2.cost_usd = None;
        mk2.observed_at = "2026-10-20T00:00:04.000Z".into();
        let sp1 = UsageEventRow {
            seq: 21,
            scope: "session".into(),
            scope_id: Some("sp1".into()),
            metric_revision: 1,
            source_session_id: Some("sp1".into()),
            mode: "snapshot".into(),
            total_tokens: Some(300),
            input_tokens: Some(300),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "reported".into(),
            observed_at: "2026-10-20T00:00:05.000Z".into(),
            observed_at_source: "native".into(),
            native_context_window: None,
            ..mk1_seq1.clone()
        };

        let fb = r46_upgrade_fixture();
        // An unrelated journal-backed instance so replay has real work.
        let other = r44_wire_record(
            "ins_r46o",
            1,
            "session",
            Some("so1"),
            Some("so1"),
            None,
            1,
            "estimated",
            [
                Basket::Known(11),
                Basket::Known(11),
                Basket::NotApplicable,
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            None,
            None,
            Some("2026-10-20T00:00:00.000Z"),
        );
        insert_journal_raw(&fb, &other);
        insert_usage_event(
            &fb,
            &project_usage_event(&other, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        // Rolled-back migration window: the new unique indexes were dropped and
        // physical duplicates accumulated.
        fb.execute_batch(
            "DROP INDEX IF EXISTS usage_events_message_dedupe;
             DROP INDEX IF EXISTS usage_events_turn_dedupe;",
        )
        .unwrap();
        let unjournaled = [&mk1_seq1, &mk1_seq2, &t1_seq10, &t1_seq11, &mk2, &sp1];
        for r in &unjournaled {
            r46_raw_insert(&fb, r, None);
        }
        let before = r43_dump_rows(&fb);
        migrate(&fb).unwrap();

        // One physical survivor per folded key, folded by the live rule.
        let (mseq, mrev, minp, mcost): (i64, i64, Option<i64>, Option<String>) = fb
            .query_row(
                "SELECT seq, metric_revision, input_tokens, cost_usd FROM usage_events
                 WHERE instance_id='ins_r46d' AND scope='message' AND scope_id='mk1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(mseq, 1, "message survivor keeps the earliest physical seq");
        assert_eq!(mrev, 2, "higher revision wins");
        assert_eq!(minp, Some(150), "rev2 counters folded, not discarded");
        assert_eq!(mcost.as_deref(), Some("0.02"));
        let (tseq, trev, tinp): (i64, i64, Option<i64>) = fb
            .query_row(
                "SELECT seq, metric_revision, input_tokens FROM usage_events
                 WHERE instance_id='ins_r46d' AND scope='turn' AND scope_id='t1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(tseq, 11, "the live turn fold advances seq");
        assert_eq!(trev, 2);
        assert_eq!(tinp, Some(110));
        let dupes: i64 = fb
            .query_row(
                "SELECT COUNT(*) FROM (
                    SELECT 1 FROM usage_events
                     WHERE instance_id='ins_r46d' AND scope_id IS NOT NULL
                     GROUP BY scope, scope_id, COALESCE(source_session_id,'')
                    HAVING COUNT(*) > 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dupes, 0);

        // Preservation: every unjournaled row OUTSIDE the two collision groups
        // is byte-identical after migration (all 21 columns). The groups'
        // survivors change by design (the captured rev2 folds onto them); the
        // folded end state is verified against the independent live reference
        // below.
        let after_once = r43_dump_rows(&fb);
        let fold_touched = [
            ("message", "1"),
            ("message", "2"),
            ("turn", "10"),
            ("turn", "11"),
        ];
        for row in &before {
            let in_fold_group = row[0].as_deref() == Some("ins_r46d")
                && fold_touched.iter().any(|(scope, seq)| {
                    row[4].as_deref() == Some(*scope) && row[1].as_deref() == Some(*seq)
                });
            if !in_fold_group && row[0].as_deref() != Some("ins_r46o") {
                assert!(
                    after_once.contains(row),
                    "unjournaled row preserved column-for-column: {row:?}"
                );
            }
        }

        // Independent live expectation: feed the same rows through the live
        // upsert in rowid order on a current-schema database.
        let lv = Connection::open_in_memory().unwrap();
        migrate(&lv).unwrap();
        create_journal_table(&lv);
        insert_journal_raw(&lv, &other);
        insert_usage_event(
            &lv,
            &project_usage_event(&other, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        for r in &unjournaled {
            insert_usage_event(&lv, r).unwrap();
        }
        migrate(&lv).unwrap();
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "folded+preserved state equals the live upsert expectation"
        );

        // (e) migrate twice == once.
        migrate(&fb).unwrap();
        assert_eq!(after_once, r43_dump_rows(&fb), "migrate twice == once");
        migrate(&lv).unwrap();
        assert_eq!(
            r43_dump_rows(&fb),
            r43_dump_rows(&lv),
            "second migration keeps both sides equal"
        );
    }

    #[test]
    fn r46_e_mixed_backed_and_unjournaled_group_keeps_the_unjournaled_revision() {
        // AE2 (i)+(ii) ordering: a durable key whose MIN(rowid) survivor is
        // journal-backed (seq1 rev1/100) but which also carries a later
        // UNJOURNALED revision (seq2 rev2/150). Folding BEFORE the backed
        // delete would merge rev2 onto the backed survivor, then delete that
        // survivor as backed and rebuild only rev1 from the journal — losing
        // rev2. The correct order deletes the backed row first; the unjournaled
        // rev2 row then stands alone and the replayed rev1 stale-loses to it,
        // exactly as live would.
        let fb = r46_upgrade_fixture();
        let rec1 = r44_wire_record(
            "ins_r46e",
            1,
            "message",
            Some("mk1"),
            None,
            Some("mt1"),
            1,
            "estimated",
            [
                Basket::Unknown,
                Basket::Known(100),
                Basket::Known(0),
                Basket::NotApplicable,
                Basket::NotApplicable,
            ],
            Some("0.01"),
            None,
            Some("2026-10-20T00:00:01.000Z"),
        );
        // seq1 is journal-backed.
        insert_journal_raw(&fb, &rec1);
        insert_usage_event(
            &fb,
            &project_usage_event(&rec1, Some("pvp"), Some("m-a")).unwrap(),
        )
        .unwrap();
        // seq2 rev2 is a later revision with NO journal frame. Drop the live
        // invariant so the two physical rows can coexist, as they would after
        // a rolled-back migration window.
        fb.execute_batch("DROP INDEX IF EXISTS usage_events_message_dedupe;")
            .unwrap();
        let rev2 = UsageEventRow {
            seq: 2,
            scope_id: Some("mk1".into()),
            turn_id: Some("mt1".into()),
            metric_revision: 2,
            total_tokens: Some(160),
            input_tokens: Some(150),
            output_tokens: Some(10),
            cost_usd: Some("0.02".into()),
            observed_at: "2026-10-20T00:00:02.000Z".into(),
            ..project_usage_event(&rec1, Some("pvp"), Some("m-a")).unwrap()
        };
        r46_raw_insert(&fb, &rev2, None);

        migrate(&fb).unwrap();
        // The unjournaled rev2 survives: the backed delete must not fold-then-
        // delete it, and the stale rev1 replay cannot lower it.
        let (rev, inp, cost, n): (i64, Option<i64>, Option<String>, i64) = fb
            .query_row(
                "SELECT metric_revision, input_tokens, cost_usd, COUNT(*) OVER ()
                 FROM usage_events WHERE instance_id='ins_r46e' AND scope='message'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(rev, 2, "unjournaled later revision preserved");
        assert_eq!(inp, Some(150), "unjournaled counters preserved");
        assert_eq!(cost.as_deref(), Some("0.02"));
        assert_eq!(n, 1, "the group collapses to one row");
        let agg = aggregate_instance(&fb, "ins_r46e").unwrap();
        assert_eq!(
            agg.input_tokens,
            Some(150),
            "accounting reads rev2, not rev1"
        );
        // Idempotent.
        let once = r43_dump_rows(&fb);
        migrate(&fb).unwrap();
        assert_eq!(once, r43_dump_rows(&fb), "migrate twice == once");
    }

    #[test]
    fn priced_flow_cost_is_billed_alongside_an_unmatched_historical_turn_ignoring_stock() {
        // c-usagefu r10 item 6 (Ruling B): COST follows the exact eligible-flow
        // decision as tokens. An unmatched historical Turn t1 ($0.10) and a NEW
        // Message r2 ($0.05, no matching Turn) are both eligible flow; r2
        // carries a priced cumulative Session snapshot ($0.99). Because an
        // eligible flow EXISTS, the priced stock must NOT be selected: the bill
        // is the unmatched Turn 0.10 + the Message 0.05 = 0.15 (and tokens are
        // the same flow 100 + 50 = 150), never the 0.99 stock.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let project = |seq: i64,
                       scope: &str,
                       scope_id: &str,
                       turn: Option<&str>,
                       out: i64,
                       cost: Option<&str>| {
            let mut event = json!({
                "kind": "usage",
                "nativeAt": {"state": "known", "value": "2030-01-01T00:00:00.000Z"},
                "payload": {
                    "scope": scope,
                    "scopeId": scope_id,
                    "mode": "snapshot",
                    "metricRevision": "1",
                    "inputTokens": {"state": "known", "value": "0"},
                    "outputTokens": {"state": "known", "value": out.to_string()},
                    "cost": if let Some(amount) = cost {
                        json!({"state": "known",
                               "value": {"amount": amount, "currency": "USD"}})
                    } else {
                        json!({"state": "unknown", "reason": "unpriced",
                               "evidenceEventIds": []})
                    },
                    "accounting": "estimated"
                }
            });
            if let Some(t) = turn {
                event["source"] = json!({"nativeTurnId": {"state": "known", "value": t}});
            }
            let rec = JournalRecord {
                instance_id: "ins_cost6".into(),
                seq,
                event_id: format!("e{seq}"),
                event,
                observed_at: "2030-01-01T00:00:00.000Z".into(),
            };
            project_usage_event(&rec, Some("pvp_cost6"), Some("gpt-5.4")).unwrap()
        };
        // Historical unmatched Turn t1.
        insert_row(
            &conn,
            &project(1, "turn", "t1", Some("t1"), 100, Some("0.10")),
            IngestEra::Live,
        )
        .unwrap();
        // New Message r2 for a turn with NO Turn row (eligible flow).
        insert_row(
            &conn,
            &project(2, "message", "resp2", Some("t2"), 50, Some("0.05")),
            IngestEra::Live,
        )
        .unwrap();
        // A priced cumulative Session snapshot that accompanies r2.
        let stock = project_usage_event(
            &tokenless_session_record_for("ins_cost6", 3, "s1", 1, Some("0.99")),
            Some("pvp_cost6"),
            Some("gpt-5.4"),
        )
        .unwrap();
        insert_usage_event(&conn, &stock).unwrap();

        let agg = aggregate_instance(&conn, "ins_cost6").unwrap();
        assert_eq!(
            agg.output_tokens,
            Some(150),
            "flow tokens 100 + 50, stock ignored"
        );
        assert!(
            (agg.cost_usd.unwrap() - 0.15).abs() < 1e-9,
            "flow cost 0.10 + 0.05, never the 0.99 priced stock, got {}",
            agg.cost_usd.unwrap()
        );
        let supply = aggregate_supply(&conn, "pvp_cost6", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, Some(150));
        assert!((supply.cost_usd.unwrap() - 0.15).abs() < 1e-9);
        let rollup = rollup_fold(&conn, "ins_cost6", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(150));
    }

    fn tokenless_session_record_for(
        instance_id: &str,
        seq: i64,
        scope_id: &str,
        revision: i64,
        cost: Option<&str>,
    ) -> JournalRecord {
        let mut rec = tokenless_session_record(seq, scope_id, revision, cost);
        rec.instance_id = instance_id.into();
        rec
    }

    fn tokenless_session_record_for_accounting(
        instance_id: &str,
        seq: i64,
        scope_id: &str,
        revision: i64,
        cost: Option<&str>,
        accounting: &str,
    ) -> JournalRecord {
        let mut rec =
            tokenless_session_record_accounting(seq, scope_id, revision, cost, accounting);
        rec.instance_id = instance_id.into();
        rec
    }

    fn priced_turn_record(
        instance_id: &str,
        seq: i64,
        scope_id: &str,
        cost: &str,
    ) -> JournalRecord {
        let mut rec = usage_record(seq, 100, Some(cost));
        rec.instance_id = instance_id.into();
        rec.event["payload"]["scopeId"] = json!(scope_id);
        rec
    }

    #[test]
    fn budget_band_uses_estimate_times_tolerance() {
        let agg = UsageAggregate {
            cost_usd: Some(4.9),
            ..Default::default()
        };
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Ok);
        let agg = UsageAggregate {
            cost_usd: Some(5.0),
            ..Default::default()
        };
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Warn);
        let agg = UsageAggregate {
            cost_usd: Some(5.76),
            ..Default::default()
        };
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Stop);
        assert_eq!(agg.budget_status(None), BudgetStatus::Ok);
        // An unknown bill is Unknown at every cap, never Ok (U1).
        let agg = UsageAggregate::default();
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Unknown);
        assert_eq!(agg.budget_status(None), BudgetStatus::Unknown);
    }

    fn ago(seconds: i64) -> String {
        let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(seconds);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.millisecond()
        )
    }

    #[test]
    fn session_snapshots_are_revision_replaced_not_frozen_or_summmed() {
        // c-ctxusage r4: cumulative session snapshots are append-only GROWTH
        // POINTS (rate windows need their history); identical stocks are
        // frozen, smaller historical stocks are frozen, growth appends.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let sess = |seq: i64, rev: i64, input: i64, output: i64| UsageEventRow {
            instance_id: "ins_g".into(),
            seq,
            profile_id: None,
            model: Some("grok-x".into()),
            scope: "session".into(),
            scope_id: Some("grok-session".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: rev,
            source_session_id: None,
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: format!("2026-10-06T00:00:0{seq}.000Z"),
            observed_at_source: "native".into(),
        };
        // Growth points at 100, then 200 — both retained.
        assert!(insert_usage_event(&conn, &sess(1, 1, 100, 10)).unwrap());
        assert!(insert_usage_event(&conn, &sess(2, 2, 200, 20)).unwrap());
        // An unchanged re-hydration (fresh seq, identical body) is frozen.
        assert!(!insert_usage_event(&conn, &sess(3, 1, 200, 20)).unwrap());
        // Adapter restart: revision back at 1, but cumulative usage grew to 300.
        assert!(insert_usage_event(&conn, &sess(4, 1, 300, 30)).unwrap());

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE instance_id='ins_g'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 3,
            "three growth points (100/200/300), one frozen duplicate"
        );
        // The current stock is the newest point (300), and the recorded
        // producer revision travels with that row.
        let (rev, input, output): (i64, i64, i64) = conn
            .query_row(
                "SELECT metric_revision, input_tokens, output_tokens
                 FROM usage_events WHERE instance_id='ins_g'
                 ORDER BY observed_at DESC, seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((rev, input, output), (1, 300, 30), "restarted growth wins");
        // Session-only totals roll up from the current stock.
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_g",
                kind: "grok",
                spec_model: Some("grok-x"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 0, "session snapshots are not turns");
        assert_eq!(rollup.session_input_tokens, Some(300));
        assert_eq!(rollup.session_output_tokens, Some(30));
    }

    #[test]
    fn turn_and_session_rows_do_not_double_count() {
        // Codex emits an additive Turn row AND a cumulative Session snapshot
        // each turn; the rollup must take the turn sums and ignore session.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let turn = |seq: i64, input: i64, output: i64| UsageEventRow {
            instance_id: "ins_c".into(),
            seq,
            profile_id: None,
            model: Some("codex".to_string()),
            scope: "turn".into(),
            scope_id: Some(format!("turn-{seq}")),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: seq,
            // Production Codex turns are qualified to their native session;
            // that lineage is what lets the session stock cover them
            // (c-ctxusage r28 item 1).
            source_session_id: Some("codex-session".into()),
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: format!("2026-10-06T00:00:0{seq}.000Z"),
            observed_at_source: "native".into(),
        };
        let cumulative = |seq: i64, input: i64, output: i64| UsageEventRow {
            scope: "session".into(),
            scope_id: Some("codex-session".into()),
            turn_id: None,
            native_context_window: None,
            metric_revision: seq,
            ..turn(seq + 10, input, output)
        };
        insert_usage_event(&conn, &turn(1, 100, 10)).unwrap();
        insert_usage_event(&conn, &cumulative(1, 100, 10)).unwrap();
        insert_usage_event(&conn, &turn(2, 50, 5)).unwrap();
        insert_usage_event(&conn, &cumulative(2, 150, 15)).unwrap();

        let rollup = rollup_fold(&conn, "ins_c", "codex", Some("codex"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.turns, 2);
        assert_eq!(
            rollup.session_input_tokens,
            Some(150),
            "the cumulative session stock equals the fully-covered turn sum"
        );
        assert_eq!(rollup.session_output_tokens, Some(15));
    }

    #[test]
    fn rollup_folds_recorded_usage_sequence() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let turns = [
            (1, 500, 4_794, 260, 29_496, 0),
            (2, 200, 1_839, 185, 33_592, 0),
            (3, 5, 1_223, 144, 34_616, 0),
        ];
        let stamps: Vec<String> = turns.iter().map(|(_, secs, ..)| ago(*secs)).collect();
        for ((seq, _, input, output, cache_read, cache_write), stamp) in
            turns.iter().zip(stamps.iter())
        {
            insert_usage_event(
                &conn,
                &row(
                    *seq,
                    stamp,
                    Some(*input),
                    Some(*output),
                    Some(*cache_read),
                    Some(*cache_write),
                ),
            )
            .unwrap();
        }

        let rollup = rollup_fold(&conn, "ins_test", "claude", None)
            .unwrap()
            .unwrap();
        assert_eq!(rollup.turns, 3);
        assert_eq!(rollup.session_input_tokens, Some(7_856));
        assert_eq!(rollup.session_output_tokens, Some(589));
        assert_eq!(rollup.cache_read_tokens, Some(97_704));
        assert_eq!(rollup.cache_creation_tokens, Some(0));
        // Last turn: 1223 fresh + 34616 cache read + 0 cache write.
        assert_eq!(rollup.context_used_tokens, Some(35_839));
        assert_eq!(rollup.context_window_tokens, Some(200_000));
        assert_eq!(rollup.context_pct, Some(18));
        assert_eq!(rollup.last_turn_at.as_deref(), Some(stamps[2].as_str()));
        // Only turn 3 is inside 60 s; turns 2+3 inside 5 min (turn 1 at
        // 500 s is outside).
        assert_eq!(rollup.tpm_in_60s, Some(1_223));
        assert_eq!(rollup.tpm_out_60s, Some(144));
        assert_eq!(
            rollup.tpm_in_5m,
            Some(((1_839 + 1_223) as f64 / 5.0).round() as i64)
        );
        assert_eq!(rollup.tpm_out_5m, Some(66));
    }

    #[test]
    fn tpm_ranks_lineages_before_applying_the_window() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let lineage_turn =
            |seq: i64, scope_id: &str, lineage: &str, input: i64, output: i64, stamp: String| {
                UsageEventRow {
                    instance_id: "ins_tpm".into(),
                    seq,
                    profile_id: None,
                    model: Some("m".into()),
                    scope: "turn".into(),
                    scope_id: Some(scope_id.into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: Some(lineage.into()),
                    total_tokens: Some(input + output),
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                    accounting: "estimated".into(),
                    observed_at: stamp,
                    observed_at_source: "native".into(),
                }
            };

        // Turn A: the in-window lineage (alpha, seq 10, 2 s ago) is superseded
        // by beta (seq 20) whose own timestamp is 500 s ago — outside every
        // rate window. Neither lineage's row may count.
        assert!(
            insert_usage_event(&conn, &lineage_turn(10, "turn-a", "alpha", 100, 10, ago(2)))
                .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &lineage_turn(20, "turn-a", "beta", 200, 20, ago(500))
            )
            .unwrap()
        );
        // Turn B (control): the winner (beta, seq 21) is in the 60 s window;
        // the older alpha row is 500 s old and loses, so beta counts once.
        assert!(
            insert_usage_event(&conn, &lineage_turn(11, "turn-b", "alpha", 11, 1, ago(500)))
                .unwrap()
        );
        assert!(
            insert_usage_event(&conn, &lineage_turn(21, "turn-b", "beta", 22, 2, ago(3))).unwrap()
        );

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_tpm",
                kind: "claude",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.tpm_in_60s,
            Some(22),
            "only turn-b's in-window winner counts; the superseded in-window \
             turn-a lineage must not"
        );
        assert_eq!(rollup.tpm_out_60s, Some(2));
        assert_eq!(rollup.tpm_in_5m, Some(4), "22 input over the 5-minute rate");
        assert_eq!(
            rollup.tpm_out_5m,
            Some(0),
            "2 output / 5 minutes rounds to 0"
        );
        // c-ctxusage r18 item 1: the context basket is the newest LINEAGE
        // WINNER, not the newest raw row. Turn-a's in-window alpha row
        // (input 100, 2 s old) is superseded by its beta winner (500 s old);
        // among the winners turn-b's 3 s old row is newest, so context is 22.
        assert_eq!(
            rollup.context_used_tokens,
            Some(22),
            "context comes from the newest winner; the superseded in-window \
             turn-a lineage must not supply a 100 context"
        );
    }

    #[test]
    fn an_ingest_time_winner_supersedes_the_native_lineage_in_tpm() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let lineage_turn = |seq: i64,
                            scope_id: &str,
                            lineage: &str,
                            input: i64,
                            output: i64,
                            stamp: String,
                            source: &str| {
            UsageEventRow {
                instance_id: "ins_tpm_prov".into(),
                seq,
                profile_id: None,
                model: Some("m".into()),
                scope: "turn".into(),
                scope_id: Some(scope_id.into()),
                turn_id: None,
                native_context_window: None,
                mode: "snapshot".into(),
                metric_revision: 1,
                source_session_id: Some(lineage.into()),
                total_tokens: Some(input + output),
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                cost_usd: None,
                accounting: "estimated".into(),
                observed_at: stamp,
                observed_at_source: source.into(),
            }
        };

        // A: older native-time row (seq 1, 100/10, 10 s ago, native) is
        // superseded by a NEWER accepted frame (seq 2, 200/20) whose time is
        // the journal ingest time — 20 s ago, ingest provenance.
        assert!(
            insert_usage_event(
                &conn,
                &lineage_turn(1, "turn-a", "alpha", 100, 10, ago(10), "native")
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &lineage_turn(2, "turn-a", "beta", 200, 20, ago(20), "ingest")
            )
            .unwrap()
        );

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_tpm_prov",
                kind: "claude",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        // The superseded native alpha row must NOT be resurrected: its 100
        // input / 10 output never enter the 60 s window.
        assert_eq!(rollup.tpm_in_60s, None, "no native-time winner contributes");
        assert_eq!(rollup.tpm_out_60s, None);
        assert_eq!(rollup.tpm_in_5m, None);
        assert_eq!(rollup.tpm_out_5m, None);
    }

    #[test]
    fn successive_session_snapshots_fold_to_the_latest_stock() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let session_row = |seq: i64, revision: i64, input: i64, output: i64| UsageEventRow {
            instance_id: "ins_stock".into(),
            seq,
            profile_id: Some("pvp".into()),
            model: Some("m".into()),
            scope: "session".into(),
            scope_id: Some("sess-1".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: revision,
            source_session_id: Some("sess-1".into()),
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: "2026-10-13T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        };
        assert!(insert_usage_event(&conn, &session_row(1, 1, 100, 0)).unwrap());
        assert!(insert_usage_event(&conn, &session_row(2, 2, 200, 0)).unwrap());

        let agg = aggregate_instance(&conn, "ins_stock").unwrap();
        assert_eq!(agg.events, 1, "one latest stock, not two snapshots");
        assert_eq!(
            agg.total_tokens,
            Some(200),
            "latest cumulative stock, not 300"
        );
        assert_eq!(agg.input_tokens, Some(200));

        let supply = aggregate_supply(&conn, "pvp", "m").unwrap();
        assert_eq!(supply.events, 1);
        assert_eq!(supply.total_tokens, Some(200));
    }

    #[test]
    fn mixed_turn_and_session_emissions_count_the_stock_once() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let row = |seq: i64,
                   scope: &str,
                   scope_id: &str,
                   lineage: Option<&str>,
                   input: i64,
                   output: i64| {
            UsageEventRow {
                instance_id: "ins_mix".into(),
                seq,
                profile_id: Some("pvp".into()),
                model: Some("m".into()),
                scope: scope.into(),
                scope_id: Some(scope_id.into()),
                turn_id: None,
                native_context_window: None,
                mode: "snapshot".into(),
                metric_revision: 1,
                source_session_id: lineage.map(str::to_string),
                total_tokens: Some(input + output),
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                cost_usd: None,
                accounting: "estimated".into(),
                observed_at: "2026-10-13T00:00:00.000Z".into(),
                observed_at_source: "native".into(),
            }
        };
        // Session "covered" emits a per-turn flow AND a cumulative stock that
        // already contains it (the Codex shape): 100/10 turn + 100/10 stock.
        assert!(
            insert_usage_event(&conn, &row(1, "turn", "msg-1", Some("covered"), 100, 10),).unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &row(2, "session", "covered", Some("covered"), 100, 10)
            )
            .unwrap()
        );
        // A second session emits ONLY a turn (no stock): its flow must count.
        assert!(
            insert_usage_event(&conn, &row(3, "turn", "msg-2", Some("uncovered"), 7, 1)).unwrap()
        );

        let agg = aggregate_instance(&conn, "ins_mix").unwrap();
        assert_eq!(
            agg.events, 2,
            "the covered stock (1) + the uncovered turn (1); the covered turn is dropped"
        );
        assert_eq!(
            agg.total_tokens,
            Some(118),
            "100+10 stock plus 7+1 turn, no double count"
        );
        assert_eq!(agg.input_tokens, Some(107));
        assert_eq!(agg.output_tokens, Some(11));

        let supply = aggregate_supply(&conn, "pvp", "m").unwrap();
        assert_eq!(supply.events, 2);
        assert_eq!(supply.total_tokens, Some(118));

        // c-ctxusage r28 item 1: the INSTANCE rollup applies the stock
        // coverage PER NATIVE SESSION — "covered"'s stock covers only its own
        // turn; the uncovered turn of another session is still added. It used
        // to return 100/10 whenever any session row existed.
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_mix",
                kind: "codex",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(107),
            "100 stock of 'covered' + 7 uncovered turn of another session"
        );
        assert_eq!(rollup.session_output_tokens, Some(11));
        assert_eq!(rollup.cache_read_tokens, Some(0));
        assert_eq!(rollup.cache_creation_tokens, Some(0));
    }

    #[test]
    fn a_turn_call_with_an_unknown_cache_bucket_has_unknown_context() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut turn = row(1, &ago(2), Some(100), Some(10), None, None);
        turn.instance_id = "ins_u1a".into();
        insert_usage_event(&conn, &turn).unwrap();

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_u1a",
                kind: "claude",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.context_used_tokens, None,
            "input 100 with unknown cache buckets is Unknown context"
        );
        assert_eq!(rollup.context_pct, None);
        // The reported input flow itself stays Known on its own bucket.
        assert_eq!(rollup.session_input_tokens, Some(100));
    }

    #[test]
    fn a_stock_with_an_unknown_cache_bucket_has_unknown_context() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut point = scoped_record(
            1,
            "ins_u1b",
            "session",
            Some("s1"),
            1,
            100,
            10,
            0,
            0,
            Some(&ago(2)),
        );
        point.event["payload"]["cacheReadTokens"] =
            json!({ "state": "unknown", "reason": "unreported" });
        point.event["payload"]["cacheWriteTokens"] =
            json!({ "state": "unknown", "reason": "unreported" });
        assert!(insert(&conn, &point));

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_u1b",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.context_used_tokens, None,
            "stock input 100 with unknown cache is Unknown context"
        );
        assert_eq!(rollup.context_pct, None);
        assert_eq!(rollup.session_input_tokens, Some(100));
        assert_eq!(rollup.session_output_tokens, Some(10));
    }

    #[test]
    fn one_session_with_unknown_input_makes_the_cross_session_total_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let s1 = scoped_record(
            1,
            "ins_u1c",
            "session",
            Some("s1"),
            1,
            100,
            10,
            0,
            0,
            Some(&ago(5)),
        );
        // s2 reports output only: its input bucket is genuinely unknown.
        let mut s2 = scoped_record(
            2,
            "ins_u1c",
            "session",
            Some("s2"),
            1,
            0,
            5,
            0,
            0,
            Some(&ago(4)),
        );
        s2.event["payload"]["inputTokens"] = json!({ "state": "unknown", "reason": "unreported" });
        assert!(insert(&conn, &s1));
        assert!(insert(&conn, &s2));

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_u1c",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.session_input_tokens, None,
            "s1's 100 cannot total with s2's unknown input"
        );
        assert_eq!(rollup.session_output_tokens, Some(15), "10 + 5 both known");

        // Control: two fully-known sessions sum to a known total.
        assert!(insert(
            &conn,
            &scoped_record(
                11,
                "ins_u1c2",
                "session",
                Some("s1"),
                1,
                100,
                10,
                0,
                0,
                Some(&ago(5))
            )
        ));
        assert!(insert(
            &conn,
            &scoped_record(
                12,
                "ins_u1c2",
                "session",
                Some("s2"),
                1,
                50,
                5,
                0,
                0,
                Some(&ago(4))
            )
        ));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_u1c2",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(150));
        assert_eq!(rollup.session_output_tokens, Some(15));
    }

    #[test]
    fn an_unknown_in_window_increment_makes_the_rate_bucket_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_u1d",
                "session",
                Some("s1"),
                1,
                100,
                10,
                0,
                0,
                Some(&ago(100))
            )
        ));
        assert!(insert(
            &conn,
            &scoped_record(
                2,
                "ins_u1d",
                "session",
                Some("s1"),
                2,
                150,
                15,
                0,
                0,
                Some(&ago(8))
            )
        ));
        let mut third = scoped_record(
            3,
            "ins_u1d",
            "session",
            Some("s1"),
            3,
            0,
            20,
            0,
            0,
            Some(&ago(3)),
        );
        third.event["payload"]["inputTokens"] =
            json!({ "state": "unknown", "reason": "unreported" });
        // The source-derived total is unknown for the same reason (U1); a
        // known total of 20 would otherwise floor as a backwards stock.
        third.event["payload"]["totalTokens"] =
            json!({ "state": "unknown", "reason": "unreported" });
        assert!(insert(&conn, &third));

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_u1d",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.tpm_in_60s, None,
            "the known +50 increment can't total past the unknown interval"
        );
        assert_eq!(rollup.tpm_out_60s, Some(10), "5 + 15->20 and 10->15 = 10");
    }

    #[test]
    fn rollup_leaves_unreported_channels_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // A Grok-like turn: output only, no input/cache breakdown.
        insert_usage_event(&conn, &row(1, &ago(2), None, Some(420), None, None)).unwrap();

        let rollup = rollup_fold(&conn, "ins_test", "grok", None)
            .unwrap()
            .unwrap();
        assert_eq!(rollup.turns, 1);
        assert_eq!(rollup.session_output_tokens, Some(420));
        assert_eq!(rollup.session_input_tokens, None);
        assert_eq!(rollup.cache_read_tokens, None);
        assert_eq!(rollup.cache_creation_tokens, None);
        assert_eq!(rollup.context_used_tokens, None);
        assert_eq!(rollup.context_window_tokens, Some(128_000));
        assert_eq!(rollup.context_pct, None);
        assert_eq!(rollup.tpm_in_60s, None);
        assert_eq!(rollup.tpm_out_60s, Some(420));

        // A generic/terminal kind has no window table either.
        let rollup = rollup_fold(&conn, "ins_test", "generic", None)
            .unwrap()
            .unwrap();
        assert_eq!(rollup.context_window_tokens, None);
        assert_eq!(rollup.context_pct, None);
    }

    #[test]
    fn rollup_no_events_means_no_rollup() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            rollup_instance(
                &conn,
                &RollupRequest {
                    instance_id: "ins_empty",
                    kind: "claude",
                    spec_model: None,
                    effective_model: None,
                    profile_id: None
                }
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn context_window_resolution_order() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let req = |kind: &'static str,
                   spec: Option<&'static str>,
                   eff: Option<&'static str>|
         -> RollupRequest<'static> {
            RollupRequest {
                instance_id: "ins_x",
                kind,
                spec_model: spec,
                effective_model: eff,
                profile_id: None,
            }
        };
        // Explicit [1m] tag wins over everything, not approximate.
        assert_eq!(
            resolve_context_window(&conn, &req("claude", None, Some("gw/model_x[1m]")), None),
            (Some(1_000_000), false)
        );
        // Catalog row for a real effective model.
        assert!(
            resolve_context_window(&conn, &req("claude", None, Some("claude-opus-5")), None)
                .0
                .is_some()
        );
        // Unknown model keeps the harness-kind fallback and is approximate.
        assert_eq!(
            resolve_context_window(&conn, &req("grok", None, Some("mystery-model")), None),
            (Some(128_000), true)
        );
        assert_eq!(
            resolve_context_window(&conn, &req("claude", None, None), None),
            (Some(200_000), true)
        );
        // Generic/terminal kind has no fallback at all.
        assert_eq!(
            resolve_context_window(&conn, &req("generic", None, None), None),
            (None, false)
        );
        // Native report beats every model-based source.
        assert_eq!(
            resolve_context_window(&conn, &req("grok", None, Some("mystery")), Some(123_456)),
            (Some(123_456), false)
        );
    }

    #[test]
    fn provider_profile_window_beats_kind_fallback_and_is_not_approximate() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "CREATE TABLE provider_profiles (
                id TEXT PRIMARY KEY,
                models_json TEXT NOT NULL DEFAULT '[]'
             );",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_profiles (id, models_json) VALUES ('pvp_test', '[{\"id\":\"gw/model-x\",\"enabled\":true,\"contextWindow\":250000}]')",
            [],
        )
        .unwrap();
        let req = RollupRequest {
            instance_id: "ins_x",
            kind: "claude",
            spec_model: Some("gw/model-x"),
            effective_model: None,
            profile_id: Some("pvp_test"),
        };
        assert_eq!(
            resolve_context_window(&conn, &req, None),
            (Some(250_000), false),
            "profile-declared window is evidence, not an estimate"
        );
    }

    #[test]
    fn provider_profile_window_beats_the_static_catalog_and_is_exact() {
        // c-usagefu r2 item 4: a gateway serving a catalog-known model at its
        // OWN window (models_json contextWindow) must not be overridden by the
        // catalog row. claude-sonnet-5 is 1,000,000 in the static catalog; the
        // gateway serves it at 200,000. Old order reported 18% at 180k used;
        // correct is 90%.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "CREATE TABLE provider_profiles (
                id TEXT PRIMARY KEY,
                models_json TEXT NOT NULL DEFAULT '[]'
             );",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_profiles (id, models_json) VALUES \
             ('pvp_sonnet', '[{\"id\":\"claude-sonnet-5\",\"enabled\":true,\"contextWindow\":200000}]')",
            [],
        )
        .unwrap();
        let req = RollupRequest {
            instance_id: "ins_x",
            kind: "claude",
            spec_model: Some("claude-sonnet-5"),
            effective_model: None,
            profile_id: Some("pvp_sonnet"),
        };
        assert_eq!(
            resolve_context_window(&conn, &req, None),
            (Some(200_000), false),
            "profile's 200k beats the catalog's 1m and is exact, not approximate"
        );
    }

    #[test]
    fn an_unresolvable_effective_model_falls_back_approximate_not_to_the_launch_window() {
        // c-usagefu r2 item 4: after a /model switch to a model the store cannot
        // resolve, the LAUNCH model's window must not be used as exact. The
        // effective id is the only candidate; resolving to nothing goes to the
        // harness-kind fallback marked approximate.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let req = RollupRequest {
            instance_id: "ins_x",
            kind: "claude",
            // Launch model has a known 1m window.
            spec_model: Some("claude-sonnet-5"),
            // Switched to something unknown.
            effective_model: Some("gateway/renamed-thing"),
            profile_id: None,
        };
        assert_eq!(
            resolve_context_window(&conn, &req, None),
            (Some(200_000), true),
            "unknown effective model -> kind fallback, approximate, never the 1m launch window"
        );

        // With NO effective model, the launch window IS consulted and is exact.
        let launch_only = RollupRequest {
            effective_model: None,
            ..req
        };
        assert_eq!(
            resolve_context_window(&conn, &launch_only, None),
            (Some(1_000_000), false),
            "spec model is consulted and exact only when no effective model exists"
        );
    }

    #[test]
    fn one_million_switch_refreshes_the_rollup_window_end_to_end() {
        // c-usagefu (c): native-time window resolution is what makes a
        // /model switch to [1m] change the chip percentage on the next read.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        insert_usage_event(
            &conn,
            &project_usage_event(
                &usage_record(1, 50_000, Some("0.01")),
                None,
                Some("passthrough/ark/seed-evolving"),
            )
            .unwrap(),
        )
        .unwrap();
        // c-ctxusage U1 merge note: the context basket is Known only when the
        // call reports every basket explicitly; the adapter always emits cache
        // zeros for Claude, so stamp them here (this test asserts WINDOW
        // resolution on a /model switch, not omitted-bucket semantics).
        let mut call = usage_record(1, 50_000, Some("0.01"));
        call.event["payload"]["cacheReadTokens"] = json!({ "state": "known", "value": "0" });
        call.event["payload"]["cacheWriteTokens"] = json!({ "state": "known", "value": "0" });
        // Re-insert is a no-op (same instance/seq); open a fresh connection so
        // the first projection carries the complete baskets.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        insert_usage_event(
            &conn,
            &project_usage_event(&call, None, Some("passthrough/ark/seed-evolving")).unwrap(),
        )
        .unwrap();
        let before = rollup_fold(
            &conn,
            "ins_test",
            "claude",
            Some("passthrough/ark/seed-evolving"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(before.context_window_tokens, Some(200_000));
        assert!(before.context_pct_approximate);
        // usage_record(1, 50_000, ..) splits to 25k uncached input.
        assert_eq!(before.context_pct, Some(13));
        // A later [1m] effective model flips the window to 1,000,000.
        let after = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_test",
                kind: "claude",
                spec_model: Some("passthrough/ark/seed-evolving"),
                effective_model: Some("passthrough/ark/seed-evolving[1m]"),
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(after.context_window_tokens, Some(1_000_000));
        assert!(!after.context_pct_approximate, "[1m] tag is evidence");
        assert_eq!(after.context_pct, Some(3), "25k/1m = 2.5 -> 3%");
    }

    // --- c-ctxusage r3 -----------------------------------------------------

    fn rollup_fold(
        conn: &Connection,
        instance_id: &str,
        kind: &str,
        model: Option<&str>,
    ) -> rusqlite::Result<Option<InstanceUsageRollup>> {
        rollup_instance(
            conn,
            &RollupRequest {
                instance_id,
                kind,
                spec_model: model,
                effective_model: None,
                profile_id: None,
            },
        )
    }

    fn known_u(n: i64) -> Value {
        json!({ "state": "known", "value": n.to_string() })
    }

    #[allow(clippy::too_many_arguments)]
    fn scoped_record(
        seq: i64,
        instance: &str,
        scope: &str,
        scope_id: Option<&str>,
        revision: i64,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        native_at: Option<&str>,
    ) -> JournalRecord {
        let total = input + output + cache_read + cache_write;
        let mut payload = serde_json::Map::new();
        payload.insert("scope".into(), json!(scope));
        payload.insert("mode".into(), json!("snapshot"));
        payload.insert("metricRevision".into(), json!(revision.to_string()));
        payload.insert("totalTokens".into(), known_u(total));
        payload.insert("inputTokens".into(), known_u(input));
        payload.insert("outputTokens".into(), known_u(output));
        payload.insert("cacheReadTokens".into(), known_u(cache_read));
        payload.insert("cacheWriteTokens".into(), known_u(cache_write));
        payload.insert(
            "cost".into(),
            json!({ "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] }),
        );
        payload.insert("accounting".into(), json!("estimated"));
        if let Some(id) = scope_id {
            payload.insert("scopeId".into(), json!(id));
        }
        let mut event = json!({ "kind": "usage", "payload": Value::Object(payload) });
        if let Some(at) = native_at {
            event["nativeAt"] = json!({ "state": "known", "value": at });
        }
        JournalRecord {
            instance_id: instance.into(),
            seq,
            event_id: format!("evt_{seq}"),
            event,
            observed_at: "2026-09-15T00:00:00.000Z".into(),
        }
    }

    fn with_source_session(mut rec: JournalRecord, session_id: &str) -> JournalRecord {
        rec.event["payload"]["sourceSessionId"] = json!(session_id);
        rec
    }

    fn insert(conn: &Connection, rec: &JournalRecord) -> bool {
        let row = project_usage_event(rec, None, None).unwrap();
        insert_usage_event(conn, &row).unwrap()
    }

    fn latest_session_input(conn: &Connection, scope_id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT input_tokens FROM usage_events
             WHERE instance_id='ins_mix' AND scope='session' AND scope_id=?1
             ORDER BY observed_at DESC, seq DESC LIMIT 1",
            params![scope_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    }

    fn session_point_count(conn: &Connection, scope_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM usage_events
             WHERE instance_id='ins_mix' AND scope='session' AND scope_id=?1",
            params![scope_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn bare_scalar_metric_revision_decodes_and_growth_points_survive_restart() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // The bare U64 string must decode (r3 item 2) — previously read through
        // knowledge_u64 and pinned to 1.
        let rev1 = scoped_record(1, "ins_mix", "session", Some("s1"), 1, 10, 5, 0, 0, None);
        let rev2 = scoped_record(2, "ins_mix", "session", Some("s1"), 2, 20, 7, 0, 0, None);
        assert_eq!(
            project_usage_event(&rev2, None, None)
                .unwrap()
                .metric_revision,
            2
        );
        assert!(insert(&conn, &rev1));
        assert!(
            insert(&conn, &rev2),
            "a larger stock appends a growth point"
        );
        assert_eq!(latest_session_input(&conn, "s1"), Some(20));

        // Adapter recreation restarts the producer counter at 1, but the
        // re-read cumulative snapshot has advanced: content acceptance is
        // independent of the reset revision (c-ctxusage r4 item 3).
        let restarted = scoped_record(3, "ins_mix", "session", Some("s1"), 1, 35, 9, 0, 0, None);
        assert!(
            insert(&conn, &restarted),
            "a restarted producer's advanced snapshot appends despite the reset revision"
        );
        assert_eq!(latest_session_input(&conn, "s1"), Some(35));

        // An unchanged re-emit (re-hydration / any revision) at a later seq is
        // frozen: no new growth point.
        let points_before = session_point_count(&conn, "s1");
        let same = scoped_record(4, "ins_mix", "session", Some("s1"), 1, 35, 9, 0, 0, None);
        assert!(!insert(&conn, &same), "identical stock never appends");
        assert_eq!(session_point_count(&conn, "s1"), points_before);

        // A smaller historical stock arriving out of order is frozen too.
        let historical = scoped_record(5, "ins_mix", "session", Some("s1"), 1, 10, 5, 0, 0, None);
        assert!(
            !insert(&conn, &historical),
            "historical smaller stock is frozen"
        );
        assert_eq!(latest_session_input(&conn, "s1"), Some(35));
    }

    #[test]
    fn real_producer_session_payload_revision_round_trips_through_project() {
        // The exact Grok/Codex adapter wire: to_usage_payload encodes
        // metric_revision as a bare U64 decimal string. project_usage_event must
        // decode it (r3 item 2), not pin it to 1.
        use remuda_driver::usage::{UsageTotals, to_usage_payload};
        use remuda_protocol::UsageScope;

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let project_one = |revision: u64, seq: i64| {
            let payload = to_usage_payload(
                UsageScope::Session,
                "grok-session-9",
                revision,
                None,
                &UsageTotals::default(),
            );
            // Emitting no counters with an empty totals still exercises the
            // revision wire; stamp counters on the JSON to force a body change.
            let mut event =
                json!({ "kind": "usage", "payload": serde_json::to_value(&payload).unwrap() });
            event["payload"]["totalTokens"] = known_u(100 * revision as i64);
            event["payload"]["inputTokens"] = known_u(100 * revision as i64);
            let rec = JournalRecord {
                instance_id: "ins_wire".into(),
                seq,
                event_id: format!("evt_w{seq}"),
                event,
                observed_at: "2026-09-15T00:00:00.000Z".into(),
            };
            project_usage_event(&rec, None, None).unwrap()
        };

        let r1 = project_one(1, 1);
        assert_eq!(r1.scope, "session");
        assert_eq!(r1.scope_id.as_deref(), Some("grok-session-9"));
        assert_eq!(r1.metric_revision, 1, "bare U64 \"1\" decodes");
        let r2 = project_one(2, 2);
        assert_eq!(
            r2.metric_revision, 2,
            "bare U64 \"2\" decodes — never pinned to 1"
        );
        assert!(insert_usage_event(&conn, &r1).unwrap());
        assert!(insert_usage_event(&conn, &r2).unwrap());
        // Two growth points; the current stock is the newest point.
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT input_tokens FROM usage_events
                 WHERE scope_id='grok-session-9'
                 ORDER BY observed_at DESC, seq DESC LIMIT 1",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            200
        );
        // The identical stock arriving again (adapter restart, fresh seq) is
        // frozen.
        assert!(
            !insert_usage_event(&conn, &r2).unwrap(),
            "dedup is by content not seq"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM usage_events WHERE scope_id='grok-session-9'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            2
        );
    }

    #[test]
    fn the_f890028e_schema_upgrades_null_revisions_and_runs_twice() {
        let conn = Connection::open_in_memory().unwrap();
        // Exact f890028e shape: scope_id + partial unique index, NO metric
        // revision column, no cache columns.
        conn.execute_batch(
            "CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                scope_id TEXT,
                mode TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );
             CREATE UNIQUE INDEX usage_events_scope_dedupe
                 ON usage_events(instance_id, scope, scope_id)
                 WHERE scope_id IS NOT NULL;
             CREATE INDEX usage_events_profile_model ON usage_events(profile_id, model);
             CREATE INDEX usage_events_instance ON usage_events(instance_id);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, mode, total_tokens,
                 input_tokens, output_tokens, accounting, observed_at)
             VALUES ('ins_old', 1, 'session', 'sess-1', 'snapshot', 100, 60, 40,
                     'estimated', '2026-09-01T00:00:00.000Z')",
            [],
        )
        .unwrap();

        // Idempotent: run the migration twice.
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let revision: i64 = conn
            .query_row(
                "SELECT metric_revision FROM usage_events WHERE scope_id='sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, 1, "a NULL revision is backfilled to 1");

        // A newer snapshot now appends as a growth point.
        let newer = scoped_record(
            2,
            "ins_old",
            "session",
            Some("sess-1"),
            2,
            80,
            50,
            0,
            0,
            None,
        );
        assert!(insert(&conn, &newer));
        let input: i64 = conn
            .query_row(
                "SELECT input_tokens FROM usage_events WHERE scope_id='sess-1'
                 ORDER BY observed_at DESC, seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(input, 80, "upgraded session rows keep growing");
    }

    #[test]
    fn codex_mixed_turn_and_session_rows_use_one_stock_flow_rule() {
        // r9 items 4/5: eligible per-call Turn flow plus a cumulative
        // Session snapshot that covers more calls. The flow sums win and
        // the stock is never a fallback; rates come from the flow alone.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let now = time::OffsetDateTime::now_utc();
        let recent = |secs_ago: i64| {
            let t = now - time::Duration::seconds(secs_ago);
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                t.millisecond()
            )
        };

        // Two per-turn flows that only PARTIALLY cover the cumulative total.
        // t1: input 100 / output 50 / cache_read 900; t2: input 100 / output 50.
        // Both are qualified to native session s1 (the production Codex
        // shape), so s1's cumulative stock covers them.
        assert!(insert(
            &conn,
            &with_source_session(
                scoped_record(
                    1,
                    "ins_mix",
                    "turn",
                    Some("t1"),
                    1,
                    100,
                    50,
                    900,
                    0,
                    Some(&recent(5))
                ),
                "s1"
            )
        ));
        assert!(insert(
            &conn,
            &with_source_session(
                scoped_record(
                    2,
                    "ins_mix",
                    "turn",
                    Some("t2"),
                    1,
                    100,
                    50,
                    0,
                    0,
                    Some(&recent(2))
                ),
                "s1"
            )
        ));
        // Cumulative session snapshot exceeds the turn sum (partial coverage):
        // input 300 / output 120.
        assert!(insert(
            &conn,
            &scoped_record(
                3,
                "ins_mix",
                "session",
                Some("s1"),
                1,
                300,
                120,
                900,
                0,
                Some(&recent(1))
            )
        ));

        let rollup = rollup_fold(&conn, "ins_mix", "codex", None)
            .unwrap()
            .unwrap();
        // r9 items 4/5: eligible flow exists, so the cumulative stock is
        // NEVER used as a smaller-flow fallback — totals are the flow sums
        // (200/100), even when the session snapshot reports more.
        assert_eq!(
            rollup.session_input_tokens,
            Some(200),
            "eligible flow sum, never the larger cumulative stock fallback"
        );
        assert_eq!(rollup.session_output_tokens, Some(100));
        assert_eq!(rollup.turns, 2);
        // Context basket = the latest TURN call (input 100), never the
        // cumulative session total (which would be 300+).
        assert_eq!(
            rollup.context_used_tokens,
            Some(100),
            "latest turn basket, not stock"
        );
        // Rate windows sum turn flows only (200), so the cumulative session
        // row is not added on top (no double count).
        assert_eq!(
            rollup.tpm_in_60s,
            Some(200),
            "TPM excludes the cumulative stock"
        );
        assert_eq!(rollup.tpm_out_60s, Some(100));
    }

    #[test]
    fn a_session_only_harness_uses_the_stock_for_totals_but_increments_for_rates() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let now = time::OffsetDateTime::now_utc();
        let stamp = |secs_ago: i64, millis: u16| {
            let t = now - time::Duration::seconds(secs_ago);
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                millis
            )
        };
        let recent = stamp(2, 900);
        // A SINGLE cumulative snapshot: totals/context are the stock, but with
        // no predecessor there is no trustworthy rate — it is unknown, never
        // "the whole stock in the last minute" (c-ctxusage r4 item 6).
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_grok",
                "session",
                Some("g1"),
                1,
                200,
                80,
                500,
                0,
                Some(&recent)
            )
        ));
        let rollup = rollup_fold(&conn, "ins_grok", "grok", None)
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(200));
        assert_eq!(
            rollup.context_used_tokens,
            Some(700),
            "stock basket 200+500"
        );
        assert_eq!(
            rollup.tpm_in_60s, None,
            "a single cumulative stock is not throughput (r4 item 6)"
        );
        assert_eq!(rollup.last_turn_at.as_deref(), Some(recent.as_str()));

        // A second snapshot grows the stock by (10 input / 4 output) one second
        // later; that increment is the in-window rate, not the new stock 210.
        let newer = stamp(1, 100);
        assert!(insert(
            &conn,
            &scoped_record(
                2,
                "ins_grok",
                "session",
                Some("g1"),
                2,
                210,
                84,
                500,
                0,
                Some(&newer)
            )
        ));
        let rollup = rollup_fold(&conn, "ins_grok", "grok", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(210),
            "latest cumulative stock"
        );
        assert_eq!(rollup.tpm_in_60s, Some(10), "increment 210-200");
        assert_eq!(rollup.tpm_out_60s, Some(4), "increment 84-80");

        // An OLD snapshot landed an hour before the newer one: an adapter
        // restart replays the EARLIER smaller stock (100, before the session
        // reached 210) at a fresh seq. Content acceptance freezes it — the
        // current 210 survives regardless of ingestion order.
        let old = stamp(3600, 0);
        let historical = scoped_record(
            10,
            "ins_grok",
            "session",
            Some("g1"),
            1,
            100,
            40,
            500,
            0,
            Some(&old),
        );
        assert!(
            !insert(&conn, &historical),
            "a smaller historical stock never clobbers the current one"
        );
        let rollup = rollup_fold(&conn, "ins_grok", "grok", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(210),
            "current stock survives a restarted historical replay"
        );
        assert_eq!(rollup.tpm_in_60s, Some(10));
    }

    #[test]
    fn real_mapper_projection_finalises_revisions_and_preserves_history() {
        use remuda_driver::TranscriptMapper;
        use remuda_protocol::{
            DriverKind, HostId, Id as PId, InstanceId, ObservationPayload, RunId,
        };

        fn assistant(id: &str, ts: &str, input: u64, stop: bool) -> String {
            serde_json::json!({
                "type": "assistant",
                "uuid": format!("{id}-rec"),
                "sessionId": "s",
                "version": "2.1.289",
                "requestId": "req-xyz",
                "timestamp": ts,
                "message": {
                    "id": id,
                    "role": "assistant",
                    "type": "message",
                    "model": "m",
                    "stop_reason": if stop { json!("end_turn") } else { Value::Null },
                    "content": [{ "type": "text", "text": "t" }],
                    "usage": {
                        "input_tokens": input,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "output_tokens": input,
                    },
                }
            })
            .to_string()
        }
        let user = serde_json::json!({
            "type": "user",
            "uuid": "u1",
            "timestamp": "2020-01-01T00:00:10.000Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "go" }] }
        })
        .to_string();

        let old = "2020-01-01T00:00:00.000Z";
        let inst = InstanceId::new();
        let inst_str = inst.as_id().to_string();
        let mut mapper = TranscriptMapper::new(
            DriverKind::ShellPty,
            inst,
            RunId::new(),
            PId::new("obj").unwrap(),
            HostId::new(),
            "s".into(),
            "2.1.289".into(),
        );

        // Feed lines + poll flushes exactly as the production pump does.
        let feed = |mapper: &mut TranscriptMapper, lines: &[String]| {
            let mut obs = Vec::new();
            for line in lines {
                obs.extend(mapper.map_line(line).unwrap());
            }
            obs.extend(mapper.flush().unwrap());
            obs
        };
        let mut observations = Vec::new();
        // msg-rev: stop then a later same-id revision.
        observations.extend(feed(&mut mapper, &[assistant("msg-rev", old, 100, true)]));
        observations.extend(feed(&mut mapper, &[assistant("msg-rev", old, 250, true)]));
        // msg-int: no stop, then a superseding user record.
        observations.extend(feed(&mut mapper, &[assistant("msg-int", old, 333, false)]));
        for o in mapper.map_line(&user).unwrap() {
            observations.push(o);
        }
        // msg-a then msg-b (different id finalises msg-a without a stop).
        observations.extend(feed(&mut mapper, &[assistant("msg-a", old, 111, false)]));
        observations.extend(feed(&mut mapper, &[assistant("msg-b", old, 222, false)]));
        observations.extend(mapper.finish().unwrap());

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // A round-one (f890028e) row already persisted under the plain id
        // msg-rev with the OLD provisional counters — item 4 legacy merge.
        let legacy = scoped_record(
            1,
            &inst_str,
            "turn",
            Some("msg-rev"),
            1,
            100,
            100,
            0,
            0,
            Some(old),
        );
        assert!(insert(&conn, &legacy), "legacy round-one row inserted");

        // Project + insert every real observation (start seq after the legacy).
        let mut seq = 2;
        let mut usage_rows = 0;
        for obs in &observations {
            if !matches!(obs.body, ObservationPayload::Usage(_)) {
                continue;
            }
            let event = serde_json::to_value(obs).unwrap();
            let rec = JournalRecord {
                instance_id: inst_str.clone(),
                seq,
                event_id: obs.event_id.clone().into(),
                event,
                observed_at: String::new(),
            };
            let row = project_usage_event(&rec, None, None).unwrap();
            insert_usage_event(&conn, &row).unwrap();
            usage_rows += 1;
            seq += 1;
        }
        assert!(usage_rows >= 3, "the mapper emitted the expected turns");

        // The WINNING (most recently accepted) durable row per plain message
        // id holds the FINAL counters. The legacy round-one row is the
        // unqualified lineage; the real mapper's qualified frames are their
        // own lineages, so msg-rev physically keeps both rows — but reads fold
        // the winner (c-ctxusage r16 item 3).
        let input_of = |id: &str| -> Option<i64> {
            conn.query_row(
                "SELECT input_tokens FROM usage_events
                 WHERE scope_id=?1 AND scope='turn'
                 ORDER BY seq DESC, rowid DESC LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .ok()
            .flatten()
        };
        assert_eq!(
            input_of("msg-rev"),
            Some(250),
            "revised final counters win over legacy/provisional across lineages"
        );
        assert_eq!(input_of("msg-int"), Some(333), "interrupted turn finalised");
        assert_eq!(input_of("msg-a"), Some(111), "superseded turn finalised");
        assert_eq!(
            input_of("msg-b"),
            Some(222),
            "end-of-stream finalises the last turn"
        );
        // One winning turn row per message id …
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM (
                     SELECT 1 FROM usage_events WHERE scope='turn'
                     GROUP BY scope_id
                 )",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            4,
            "one logical turn per message id across lineages"
        );
        // … while the legacy unqualified row for msg-rev survives physically
        // in its own lineage (the NULL mark must not be erased).
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM usage_events WHERE scope='turn'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            5,
            "msg-rev keeps both its NULL-lineage legacy row and the qualified row"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT metric_revision FROM usage_events
                 WHERE scope_id='msg-rev' AND source_session_id IS NULL",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            1,
            "the unqualified lineage keeps its own watermark"
        );

        // Item 5: everything is historical → totals rise but the rate window is
        // empty and lastTurnAt stays in the past.
        let rollup = rollup_fold(&conn, &inst_str, "claude", Some("m"))
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(916),
            "250+333+111+222 all counted"
        );
        assert_eq!(
            rollup.tpm_in_60s, None,
            "historical replay excluded from TPM"
        );
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some(old),
            "lastTurnAt stays historical"
        );
    }
    // --- c-ctxusage r4 items 3, 4, 5 --------------------------------------

    #[test]
    fn session_stock_replay_is_content_ordered_in_either_arrival_order() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Direction 1: an interrupted replay lands the small historical stock
        // first (fresh seq), then the current stock catches up.
        assert!(insert(
            &conn,
            &scoped_record(
                100,
                "ins_r3",
                "session",
                Some("s"),
                1,
                100,
                10,
                0,
                0,
                Some("2020-01-01T00:00:00.000Z")
            )
        ));
        assert!(insert(
            &conn,
            &scoped_record(
                101,
                "ins_r3",
                "session",
                Some("s"),
                1,
                1000,
                100,
                0,
                0,
                Some("2020-01-01T00:01:00.000Z")
            )
        ));
        let current: i64 = conn
            .query_row(
                "SELECT input_tokens FROM usage_events
                 WHERE instance_id='ins_r3' AND scope='session' AND scope_id='s'
                 ORDER BY observed_at DESC, seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            current, 1000,
            "the current stock wins even when it arrives last"
        );

        // Direction 2: current exists, a smaller historical replay lands later.
        assert!(!insert(
            &conn,
            &scoped_record(
                200,
                "ins_r3",
                "session",
                Some("s"),
                1,
                100,
                10,
                0,
                0,
                Some("2020-01-01T00:00:00.000Z")
            )
        ));
        assert!(!insert(
            &conn,
            &scoped_record(
                201,
                "ins_r3",
                "session",
                Some("s"),
                1,
                500,
                50,
                0,
                0,
                Some("2020-01-01T00:00:30.000Z")
            )
        ));
        let current: i64 = conn
            .query_row(
                "SELECT input_tokens FROM usage_events
                 WHERE instance_id='ins_r3' AND scope='session' AND scope_id='s'
                 ORDER BY observed_at DESC, seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            current, 1000,
            "a smaller historical stock never clobbers current"
        );
    }

    #[test]
    fn context_basket_follows_native_time_not_ingest_seq() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Message A (older): provisional 100 at seq 10.
        assert!(insert(
            &conn,
            &scoped_record(
                10,
                "ins_r4",
                "turn",
                Some("A"),
                1,
                100,
                5,
                0,
                0,
                Some("2020-01-01T00:00:00.000Z")
            )
        ));
        // Message B (newer): 200 at seq 20 — the current call.
        assert!(insert(
            &conn,
            &scoped_record(
                20,
                "ins_r4",
                "turn",
                Some("B"),
                1,
                200,
                7,
                0,
                0,
                Some("2020-01-01T00:00:05.000Z")
            )
        ));
        // Legacy replay corrects A to 150 at the high seq 100. Turn upsert
        // revises A in place (its stored seq becomes 100).
        assert!(insert(
            &conn,
            &scoped_record(
                100,
                "ins_r4",
                "turn",
                Some("A"),
                2,
                150,
                5,
                0,
                0,
                Some("2020-01-01T00:00:00.000Z")
            )
        ));
        let rollup = rollup_fold(&conn, "ins_r4", "claude", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.context_used_tokens,
            Some(200),
            "context stays on the newer message B (input 200), not the high-seq correction to A"
        );
    }

    #[test]
    fn unchanged_counters_at_a_newer_position_repair_native_time_and_empty_windows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // First delivery: no nativeAt — observed_at is the ingest fallback
        // ("now"), so the row sits inside the rate windows.
        let now = {
            let t = time::OffsetDateTime::now_utc();
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                t.millisecond()
            )
        };
        let ingest = JournalRecord {
            instance_id: "ins_r5".into(),
            seq: 1,
            event_id: "evt_1".into(),
            event: json!({
                "kind": "usage",
                "payload": {
                    "scope": "turn",
                    "mode": "snapshot",
                    "scopeId": "msg-x",
                    "metricRevision": "1",
                    "totalTokens": { "state": "known", "value": "250" },
                    "inputTokens": { "state": "known", "value": "250" },
                    "outputTokens": { "state": "known", "value": "0" },
                    "cacheReadTokens": { "state": "known", "value": "0" },
                    "cacheWriteTokens": { "state": "known", "value": "0" },
                    "cost": { "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] },
                    "accounting": "estimated"
                }
            }),
            observed_at: now.clone(),
        };
        let row = project_usage_event(&ingest, None, None).unwrap();
        assert_eq!(row.observed_at_source, "ingest");
        assert!(insert_usage_event(&conn, &row).unwrap());
        let rollup = rollup_fold(&conn, "ins_r5", "claude", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.tpm_in_60s, None,
            "c-ctxusage r5 item 7: an ingest-fallback row is not current throughput; \
             the real time is unknown until a native-timed observation repairs it"
        );
        assert_eq!(
            rollup.session_input_tokens,
            Some(250),
            "totals still count ingest rows"
        );

        // Newer source position: identical counters, now with the real
        // historical native time. The watermark advances and the clock is
        // repaired.
        let historical = scoped_record(
            2,
            "ins_r5",
            "turn",
            Some("msg-x"),
            2,
            250,
            0,
            0,
            0,
            Some("2020-01-01T00:00:00.000Z"),
        );
        assert!(
            insert(&conn, &historical),
            "a newer native snapshot is applied even with identical counters"
        );
        let (at, source): (String, String) = conn
            .query_row(
                "SELECT observed_at, observed_at_source FROM usage_events
                 WHERE instance_id='ins_r5' AND scope_id='msg-x'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(at, "2020-01-01T00:00:00.000Z");
        assert_eq!(source, "native");

        let rollup = rollup_fold(&conn, "ins_r5", "claude", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some("2020-01-01T00:00:00.000Z"),
            "lastTurnAt repaired to the native time"
        );
        assert_eq!(
            rollup.tpm_in_60s, None,
            "historical tokens leave the current rate window"
        );
        assert_eq!(rollup.session_input_tokens, Some(250), "counters unchanged");
    }

    #[test]
    fn an_equal_position_native_replay_is_frozen_a_newer_one_repairs() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // First delivery carries no nativeAt: observed_at is the ingest
        // fallback, stamped with the helper's ingest string.
        let ingested = scoped_record(
            1,
            "ins_r11t5",
            "turn",
            Some("msg-eq"),
            1,
            250,
            0,
            0,
            0,
            None,
        );
        assert!(insert(&conn, &ingested));
        let source_before: String = conn
            .query_row(
                "SELECT observed_at_source FROM usage_events
                 WHERE instance_id='ins_r11t5' AND scope_id='msg-eq'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source_before, "ingest");

        // Native replay at the SAME source position, native timestamp EQUAL to
        // the stored ingest fallback string: frozen, clock and provenance
        // untouched.
        let at = "2026-09-15T00:00:00.000Z";
        let equal = scoped_record(
            2,
            "ins_r11t5",
            "turn",
            Some("msg-eq"),
            1,
            250,
            0,
            0,
            0,
            Some(at),
        );
        assert!(
            !insert(&conn, &equal),
            "an equal-position native replay must not rewrite the clock"
        );
        let (observed_at, source, seq): (String, String, i64) = conn
            .query_row(
                "SELECT observed_at, observed_at_source, seq FROM usage_events
                 WHERE instance_id='ins_r11t5' AND scope_id='msg-eq'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())),
            )
            .unwrap();
        let _ = observed_at;
        assert_eq!(
            source, "ingest",
            "an equal position never repairs provenance"
        );
        assert_eq!(seq, 1, "the frozen replay must not adopt its seq");
        // The timestamp STRING happens to equal the fallback; what proves the
        // freeze is that provenance is still ingest (checked above): no clock
        // rewrite occurred at an equal position.

        // A STRICTLY NEWER position carrying the native time repairs it.
        let repaired = scoped_record(
            3,
            "ins_r11t5",
            "turn",
            Some("msg-eq"),
            2,
            250,
            0,
            0,
            0,
            Some(at),
        );
        assert!(
            insert(&conn, &repaired),
            "a newer native snapshot repairs the provenance"
        );
        let (observed, source, seq): (String, String, i64) = conn
            .query_row(
                "SELECT observed_at, observed_at_source, seq FROM usage_events
                 WHERE instance_id='ins_r11t5' AND scope_id='msg-eq'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())),
            )
            .unwrap();
        assert_eq!(observed, at);
        assert_eq!(source, "native", "the newer position repairs provenance");
        assert_eq!(seq, 3, "the repair adopts the replay's seq");

        // A subsequent identical native replay at the same repaired position
        // is a genuine no-op.
        let again = scoped_record(
            4,
            "ins_r11t5",
            "turn",
            Some("msg-eq"),
            2,
            250,
            0,
            0,
            0,
            Some(at),
        );
        assert!(
            !insert(&conn, &again),
            "native -> native at the same position with identical counters/time changes nothing"
        );
    }
    #[test]
    fn the_watermark_advances_on_an_unchanged_newer_snapshot() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // rev 1: input 100 / output 20.
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_r14w3",
                "turn",
                Some("msg-w3"),
                1,
                100,
                20,
                0,
                0,
                Some("2026-10-10T00:00:00.000Z")
            )
        ));
        // rev 3: SAME counters, newer source position. Must apply (watermark
        // advances) even though nothing moved.
        assert!(
            insert(
                &conn,
                &scoped_record(
                    2,
                    "ins_r14w3",
                    "turn",
                    Some("msg-w3"),
                    3,
                    100,
                    20,
                    0,
                    0,
                    Some("2026-10-10T00:00:10.000Z")
                )
            ),
            "an unchanged newer snapshot must still persist its watermark"
        );
        // rev 2 / output 15: now STALE, and a decrease on top of that — frozen.
        assert!(
            !insert(
                &conn,
                &scoped_record(
                    3,
                    "ins_r14w3",
                    "turn",
                    Some("msg-w3"),
                    2,
                    100,
                    15,
                    0,
                    0,
                    Some("2026-10-10T00:00:05.000Z")
                )
            ),
            "the older position must not roll output back to 15"
        );
        let (revision, output): (i64, Option<i64>) = conn
            .query_row(
                "SELECT metric_revision, output_tokens FROM usage_events
                 WHERE instance_id='ins_r14w3' AND scope_id='msg-w3'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(revision, 3, "the watermark persisted at 3");
        assert_eq!(output, Some(20), "output stays 20 after the stale frame");
    }

    #[test]
    fn an_equal_position_snapshot_never_grows_or_fills_a_bucket() {
        // A turn frame that reports only input/output (cache buckets omitted =
        // Unknown, never zero).
        let partial_record = |seq: i64, revision: i64, input: i64, output: i64| {
            let mut payload = serde_json::Map::new();
            payload.insert("scope".into(), json!("turn"));
            payload.insert("mode".into(), json!("snapshot"));
            payload.insert("scopeId".into(), json!("msg-eq2"));
            payload.insert("metricRevision".into(), json!(revision.to_string()));
            payload.insert("inputTokens".into(), known_u(input));
            payload.insert("outputTokens".into(), known_u(output));
            payload.insert(
                "cost".into(),
                json!({ "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] }),
            );
            payload.insert("accounting".into(), json!("estimated"));
            JournalRecord {
                instance_id: "ins_r15i2".into(),
                seq,
                event_id: format!("evt_{seq}"),
                event: json!({ "kind": "usage", "payload": Value::Object(payload) }),
                observed_at: "2026-09-15T00:00:00.000Z".into(),
            }
        };
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // rev 3: input 100 / output 20, cache buckets omitted (Unknown).
        assert!(insert(&conn, &partial_record(1, 3, 100, 20)));
        // The SAME rev 3 with output 25 (a growth): must change nothing.
        assert!(
            !insert(&conn, &partial_record(2, 3, 100, 25)),
            "an equal-position growth must be rejected outright"
        );

        // An equal-position FILL: still rev 3, now reporting a cache_read
        // bucket. Retained buckets are filled only by a STRICTLY newer
        // position; an equal one must leave them Unknown.
        let mut fill_payload = serde_json::Map::new();
        fill_payload.insert("scope".into(), json!("turn"));
        fill_payload.insert("mode".into(), json!("snapshot"));
        fill_payload.insert("scopeId".into(), json!("msg-eq2"));
        fill_payload.insert("metricRevision".into(), json!("3"));
        fill_payload.insert("inputTokens".into(), known_u(100));
        fill_payload.insert("outputTokens".into(), known_u(20));
        fill_payload.insert(
            "cacheReadTokens".into(),
            json!({ "state": "known", "value": "7" }),
        );
        fill_payload.insert(
            "cost".into(),
            json!({ "state": "unknown", "reason": "unpriced", "evidenceEventIds": [] }),
        );
        fill_payload.insert("accounting".into(), json!("estimated"));
        let equal_fill = JournalRecord {
            instance_id: "ins_r15i2".into(),
            seq: 3,
            event_id: "evt_3".into(),
            event: json!({ "kind": "usage", "payload": Value::Object(fill_payload) }),
            observed_at: "2026-09-15T00:00:00.000Z".into(),
        };
        assert!(
            !insert(&conn, &equal_fill),
            "an equal-position bucket fill must be rejected outright"
        );

        let (revision, input, output, cache_read, seq): (
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            i64,
        ) = conn
            .query_row(
                "SELECT metric_revision, input_tokens, output_tokens,
                        cache_read_tokens, seq
                 FROM usage_events
                 WHERE instance_id='ins_r15i2' AND scope_id='msg-eq2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(revision, 3);
        assert_eq!(input, Some(100));
        assert_eq!(
            output,
            Some(20),
            "the equal-position 20→25 growth is frozen"
        );
        assert_eq!(cache_read, None, "the equal-position bucket fill is frozen");
        assert_eq!(seq, 1, "neither equal-position frame adopts its seq");
    }

    #[test]
    fn a_cost_only_frame_is_position_gated_advances_the_watermark_and_keeps_counters_and_clock() {
        // A cost-only turn frame: scope/scopeId/revision + bill, NO token
        // baskets at all.
        let cost_only = |seq: i64, revision: i64, amount: &str| {
            let mut payload = serde_json::Map::new();
            payload.insert("scope".into(), json!("turn"));
            payload.insert("mode".into(), json!("snapshot"));
            payload.insert("scopeId".into(), json!("msg-co"));
            payload.insert("metricRevision".into(), json!(revision.to_string()));
            payload.insert(
                "cost".into(),
                json!({
                    "state": "known",
                    "value": { "amount": amount, "currency": "USD" }
                }),
            );
            payload.insert("accounting".into(), json!("estimated"));
            JournalRecord {
                instance_id: "ins_r15i3".into(),
                seq,
                event_id: format!("evt_{seq}"),
                event: json!({ "kind": "usage", "payload": Value::Object(payload) }),
                observed_at: "2026-09-15T00:00:00.000Z".into(),
            }
        };
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // rev 3: input 100 / output 20 at a native time, no bill.
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_r15i3",
                "turn",
                Some("msg-co"),
                3,
                100,
                20,
                0,
                0,
                Some("2026-10-10T00:00:00.000Z")
            )
        ));

        // Stale cost-only correction at rev 1 (older than the watermark):
        // changes nothing — the bill is not corrected and the watermark
        // stays at 3.
        assert!(
            !insert(&conn, &cost_only(2, 1, "0.400000000")),
            "a stale cost-only frame must be rejected by the position gate"
        );
        let row: (i64, Option<String>) = conn
            .query_row(
                "SELECT metric_revision, cost_usd FROM usage_events
                 WHERE instance_id='ins_r15i3' AND scope_id='msg-co'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (3, None), "the stale cost correction never lands");

        // Strictly newer cost-only at rev 5: corrects the bill and advances
        // the watermark. Counters and the native turn clock are kept.
        assert!(
            insert(&conn, &cost_only(3, 5, "0.500000000")),
            "a newer cost-only frame corrects the bill"
        );
        let row: (
            i64,
            Option<i64>,
            Option<i64>,
            Option<String>,
            String,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT metric_revision, input_tokens, output_tokens, cost_usd,
                        observed_at, observed_at_source, seq
                 FROM usage_events
                 WHERE instance_id='ins_r15i3' AND scope_id='msg-co'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, 5, "the cost-only frame advances the watermark to 5");
        assert_eq!((row.1, row.2), (Some(100), Some(20)), "counters kept");
        assert_eq!(row.3.as_deref(), Some("0.500000000"), "bill corrected");
        assert_eq!(
            (row.4.as_str(), row.5.as_str()),
            ("2026-10-10T00:00:00.000Z", "native"),
            "a cost-only frame never rewrites the turn clock"
        );
        assert_eq!(row.6, 3, "the billing event advances the ingestion seq");

        // A counter snapshot at rev 4 is now STALE against the cost-only
        // watermark 5: rejected, output stays 20 even though 25 is a growth.
        assert!(
            !insert(
                &conn,
                &scoped_record(
                    4,
                    "ins_r15i3",
                    "turn",
                    Some("msg-co"),
                    4,
                    100,
                    25,
                    0,
                    0,
                    Some("2026-10-10T00:00:04.000Z")
                )
            ),
            "the cost-only watermark rejects a later older-position snapshot"
        );
        let output: Option<i64> = conn
            .query_row(
                "SELECT output_tokens FROM usage_events
                 WHERE instance_id='ins_r15i3' AND scope_id='msg-co'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            output,
            Some(20),
            "the stale counter snapshot changes nothing"
        );
    }

    #[test]
    fn session_qualified_watermark_migrates_legacy_rows_to_the_null_lineage() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Legacy unqualified row already at revision 3 (as older producers
        // wrote it).
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_r15i4",
                "turn",
                Some("msg-legacy"),
                3,
                100,
                20,
                0,
                0,
                Some("2026-10-10T00:00:00.000Z")
            )
        ));
        let lineage: Option<String> = conn
            .query_row(
                "SELECT source_session_id FROM usage_events
                 WHERE instance_id='ins_r15i4' AND scope_id='msg-legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(lineage, None, "existing rows are the unqualified lineage");

        // Another unqualified frame at rev 2 keeps the numeric gate: frozen.
        assert!(
            !insert(
                &conn,
                &scoped_record(
                    2,
                    "ins_r15i4",
                    "turn",
                    Some("msg-legacy"),
                    2,
                    100,
                    19,
                    0,
                    0,
                    Some("2026-10-10T00:00:00.000Z")
                )
            ),
            "two unqualified frames share the numeric watermark"
        );

        // A qualified frame for a real session at rev 1 is a DIFFERENT
        // lineage over the same message id: accepted into its OWN row.
        let mut qualified = scoped_record(
            3,
            "ins_r15i4",
            "turn",
            Some("msg-legacy"),
            1,
            90,
            9,
            0,
            0,
            Some("2026-10-10T00:00:01.000Z"),
        );
        qualified.event["payload"]["sourceSessionId"] = json!("sess-rebound");
        assert!(
            insert(&conn, &qualified),
            "a qualified frame over the legacy NULL lineage is not numerically frozen"
        );

        // Two physical rows, one per lineage: the qualified acceptance did
        // NOT erase the NULL mark or rewrite its identity (T1').
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id='ins_r15i4' AND scope_id='msg-legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row_count, 2, "each lineage keeps its own revision row");
        let (revision, lineage, output): (i64, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT metric_revision, source_session_id, output_tokens FROM usage_events
                 WHERE instance_id='ins_r15i4' AND scope_id='msg-legacy'
                   AND source_session_id = 'sess-rebound'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(revision, 1);
        assert_eq!(lineage.as_deref(), Some("sess-rebound"));
        assert_eq!(output, Some(9));
        let null_revision: i64 = conn
            .query_row(
                "SELECT metric_revision FROM usage_events
                 WHERE instance_id='ins_r15i4' AND scope_id='msg-legacy'
                   AND source_session_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_revision, 3, "the NULL lineage keeps its own mark");
    }

    fn qualified_record(
        seq: i64,
        instance: &str,
        scope_id: &str,
        session: &str,
        revision: i64,
        input: i64,
        output: i64,
    ) -> JournalRecord {
        let mut record = scoped_record(
            seq,
            instance,
            "turn",
            Some(scope_id),
            revision,
            input,
            output,
            0,
            0,
            Some("2026-10-10T00:00:00.000Z"),
        );
        record.event["payload"]["sourceSessionId"] = json!(session);
        record
    }

    #[test]
    fn accepting_another_lineage_never_erases_the_first_lineages_watermark() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(insert(
            &conn,
            &qualified_record(1, "ins_t1p_a", "msg-1", "sess-alpha", 3, 100, 30)
        ));
        assert!(insert(
            &conn,
            &qualified_record(2, "ins_t1p_a", "msg-1", "sess-beta", 1, 50, 5)
        ));
        assert!(
            !insert(
                &conn,
                &qualified_record(3, "ins_t1p_a", "msg-1", "sess-alpha", 2, 90, 25)
            ),
            "alpha position 2 is stale against alpha's OWN mark 3, beta never erased it"
        );
        let marks = conn
            .prepare(
                "SELECT source_session_id, metric_revision, output_tokens
                 FROM usage_events
                 WHERE instance_id='ins_t1p_a' AND scope_id='msg-1'
                 ORDER BY source_session_id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            marks,
            vec![
                ("sess-alpha".to_string(), 3, Some(30)),
                ("sess-beta".to_string(), 1, Some(5)),
            ],
            "both lineages keep their own marks and counters"
        );
    }

    #[test]
    fn restart_keeps_each_lineage_watermark_and_a_stale_replay_still_loses() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Native session A at revision 100 / seq 1.
        assert!(insert(
            &conn,
            &qualified_record(1, "ins_r37i1", "msg-T", "sess-A", 100, 100, 10)
        ));
        // The SAME turn from session B at revision 1 / seq 2 with IDENTICAL
        // buckets: two legitimate rows only under the per-lineage key.
        assert!(insert(
            &conn,
            &qualified_record(2, "ins_r37i1", "msg-T", "sess-B", 1, 100, 10)
        ));

        // Hub reopen: the whole migration runs again.
        migrate(&conn).unwrap();

        let marks = conn
            .prepare(
                "SELECT source_session_id, metric_revision, seq
                 FROM usage_events
                 WHERE instance_id='ins_r37i1' AND scope_id='msg-T'
                 ORDER BY source_session_id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            marks,
            vec![("sess-A".to_string(), 100, 1), ("sess-B".to_string(), 1, 2),],
            "reopen keeps BOTH lineage watermarks, positions and seqs"
        );

        // A stale replay of A at revision 50 must still lose against A's own
        // retained mark 100 — it must not find an empty lineage and win.
        assert!(
            !insert(
                &conn,
                &qualified_record(3, "ins_r37i1", "msg-T", "sess-A", 50, 100, 10)
            ),
            "the stale A replay is frozen by A's surviving watermark"
        );
        let a_revision: i64 = conn
            .query_row(
                "SELECT metric_revision FROM usage_events
                 WHERE instance_id='ins_r37i1' AND scope_id='msg-T'
                   AND source_session_id='sess-A'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_revision, 100, "A's watermark never moved");
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id='ins_r37i1' AND scope_id='msg-T'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row_count, 2, "the rejected replay added no row");
    }

    #[test]
    fn a_migrated_lineage_qualified_turn_is_not_also_counted_as_legacy() {
        // The c-ctxusage r28 shape: every column except the merge-introduced
        // lineage_state (and usagefu's turn_id / native_context_window).
        let create_r28 = "CREATE TABLE usage_events (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            profile_id TEXT,
            model TEXT,
            scope TEXT NOT NULL,
            scope_id TEXT,
            mode TEXT NOT NULL,
            total_tokens INTEGER,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            cost_usd TEXT,
            accounting TEXT NOT NULL DEFAULT 'estimated',
            observed_at TEXT NOT NULL,
            observed_at_source TEXT NOT NULL DEFAULT 'ingest',
            metric_revision INTEGER NOT NULL DEFAULT 1,
            source_session_id TEXT,
            PRIMARY KEY (instance_id, seq)
         );";
        let at = "2026-10-13T00:00:00.000Z";

        // Instance A: one qualified turn, NO scoped stock — the legacy-OPEN
        // overlap (double counted input/cost).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(create_r28).unwrap();
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                 observed_at, observed_at_source, metric_revision,
                 source_session_id)
             VALUES ('ins_r37i2a', 1, 'pvp', 'm', 'turn', 'msg-1', 'snapshot',
                 110, 100, 10, 0, 0, '1.00', 'estimated',
                 ?1, 'native', 1, 'sess-1')",
            params![at],
        )
        .unwrap();

        // Instance B: a qualified turn beside an UNRELATED session's stock
        // plus a live unqualified turn — the legacy-FLOW overlap.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                 observed_at, observed_at_source, metric_revision,
                 source_session_id)
             VALUES ('ins_r37i2b', 1, 'pvp', 'm', 'turn', 'msg-1', 'snapshot',
                 110, 100, 10, 0, 0, '1.00', 'estimated',
                 ?1, 'native', 1, 'sess-1')",
            params![at],
        )
        .unwrap();
        // Cumulative stock for a DIFFERENT session than the turn's lineage.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                 observed_at, observed_at_source, metric_revision,
                 source_session_id)
             VALUES ('ins_r37i2b', 2, 'pvp', 'm', 'session', 'sess-2',
                 'snapshot',
                 55, 50, 5, 0, 0, '0.50', 'estimated',
                 ?1, 'native', 1, 'sess-2')",
            params![at],
        )
        .unwrap();

        // The real upgrade: retained rows are tagged, lineage retained.
        migrate(&conn).unwrap();
        let (state, sid): (String, Option<String>) = conn
            .query_row(
                "SELECT lineage_state, source_session_id FROM usage_events
                 WHERE instance_id='ins_r37i2a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "legacy");
        assert_eq!(
            sid.as_deref(),
            Some("sess-1"),
            "the lineage survives the tag"
        );

        // The other per-call on instance B: a LIVE unqualified turn (the live
        // insert path leaves lineage_state NULL).
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r37i2b".into(),
                    seq: 3,
                    profile_id: Some("pvp".into()),
                    model: Some("m".into()),
                    scope: "turn".into(),
                    scope_id: Some("msg-2".into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: None,
                    total_tokens: Some(22),
                    input_tokens: Some(20),
                    output_tokens: Some(2),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: Some("0.20".into()),
                    accounting: "estimated".into(),
                    observed_at: at.into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );

        // Instance A: the one qualified turn contributes exactly once.
        let a = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r37i2a",
                kind: "claude",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(a.turns, 1, "one turn, not proven+legacy two");
        assert_eq!(a.session_input_tokens, Some(100), "100, not 200");
        assert_eq!(a.session_output_tokens, Some(10));
        assert_eq!(a.cache_read_tokens, Some(0));
        assert_eq!(a.cache_creation_tokens, Some(0));

        // Instance B: qualified turn (100/10) plus the live unqualified turn
        // (20/2) each once; the unrelated sess-2 stock stands down under the
        // live turn's Ruling-B override, but the qualified turn must never be
        // counted a second time through legacy-flow.
        let b = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r37i2b",
                kind: "claude",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(b.turns, 2, "two turns, not a doubled qualified turn");
        assert_eq!(b.session_input_tokens, Some(120), "100 + 20, not 220");
        assert_eq!(b.session_output_tokens, Some(12), "10 + 2, not 22");
    }

    #[test]
    fn recovered_turn_collapse_keeps_the_higher_revision_over_a_later_stale_replay() {
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        // Established row: seq1 rev100 input100.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at, observed_at_source)
             VALUES ('ins_r39i1a', 1, 'pvp', 'm', 'turn', 'msg-T', 'snapshot',
                 100, 'sess-S', 100, 100, 0, 0, 0,
                 'estimated', '2026-10-01T00:00:00.000Z', 'native')",
            [],
        )
        .unwrap();
        // Fallback row at seq2 (14-column shape): stale rev99 input50.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, total_tokens,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at)
             VALUES ('ins_r39i1a', 2, 'pvp', 'm', 'turn', 'snapshot', 50,
                 50, 0, 0, 0,
                 'estimated', '2026-10-01T00:00:02.000Z')",
            [],
        )
        .unwrap();
        let journal_turn = |revision: i64, input: i64, stamp: &str| {
            json!({
                "kind": "usage",
                "nativeAt": {"state": "known", "value": stamp},
                "source": {
                    "nativeSessionId": {"state": "known", "value": "sess-S"},
                    "nativeTurnId": {"state": "known", "value": "msg-T"}
                },
                "payload": {
                    "scope": "turn",
                    "scopeId": "msg-T",
                    "mode": "snapshot",
                    "metricRevision": revision.to_string(),
                    "accounting": "estimated",
                    "inputTokens": {"state": "known", "value": input.to_string()},
                    "outputTokens": {"state": "known", "value": "0"},
                    "cacheReadTokens": {"state": "known", "value": "0"},
                    "cacheWriteTokens": {"state": "known", "value": "0"},
                    "totalTokens": {"state": "known", "value": input.to_string()}
                }
            })
        };
        for (seq, rev, input, stamp) in [
            (1i64, 100i64, 100i64, "2026-10-01T00:00:00.000Z"),
            (2, 99, 50, "2026-10-01T00:00:02.000Z"),
        ] {
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES ('ins_r39i1a', ?1, ?2, ?3, ?4)",
                params![
                    seq,
                    format!("je{seq}"),
                    journal_turn(rev, input, stamp).to_string(),
                    stamp
                ],
            )
            .unwrap();
        }

        migrate(&conn).unwrap();

        let (count, revision, input): (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(metric_revision), MAX(input_tokens)
                 FROM usage_events WHERE instance_id='ins_r39i1a' AND scope='turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "one turn survives");
        assert_eq!(revision, 100, "the higher REVISION wins, regardless of seq");
        assert_eq!(
            input,
            Some(100),
            "the stale rev99/50 replay never replaces it"
        );
    }

    #[test]
    fn recovered_turn_collapse_merges_known_buckets_into_a_newer_partial_frame() {
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        // Complete rev100 frame: input100, cache read 50, total 150.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at, observed_at_source)
             VALUES ('ins_r39i1b', 1, 'pvp', 'm', 'turn', 'msg-P', 'snapshot',
                 100, 'sess-S', 150, 100, 0, 50, 0,
                 'estimated', '2026-10-01T00:00:00.000Z', 'native')",
            [],
        )
        .unwrap();
        // Newer rev101 frame through the fallback path reports ONLY input 100:
        // every other bucket and the total are unreported (NULL).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, input_tokens,
                 accounting, observed_at)
             VALUES ('ins_r39i1b', 2, 'pvp', 'm', 'turn', 'snapshot', 100,
                 'estimated', '2026-10-01T00:00:02.000Z')",
            [],
        )
        .unwrap();
        let journal_full = json!({
            "kind": "usage",
            "nativeAt": {"state": "known", "value": "2026-10-01T00:00:00.000Z"},
            "source": {
                "nativeSessionId": {"state": "known", "value": "sess-S"},
                "nativeTurnId": {"state": "known", "value": "msg-P"}
            },
            "payload": {
                "scope": "turn",
                "scopeId": "msg-P",
                "mode": "snapshot",
                "metricRevision": "100",
                "accounting": "estimated",
                "inputTokens": {"state": "known", "value": "100"},
                "outputTokens": {"state": "known", "value": "0"},
                "cacheReadTokens": {"state": "known", "value": "50"},
                "cacheWriteTokens": {"state": "known", "value": "0"},
                "totalTokens": {"state": "known", "value": "150"}
            }
        });
        let journal_partial = json!({
            "kind": "usage",
            "nativeAt": {"state": "known", "value": "2026-10-01T00:00:02.000Z"},
            "source": {
                "nativeSessionId": {"state": "known", "value": "sess-S"},
                "nativeTurnId": {"state": "known", "value": "msg-P"}
            },
            "payload": {
                "scope": "turn",
                "scopeId": "msg-P",
                "mode": "snapshot",
                "metricRevision": "101",
                "accounting": "estimated",
                "inputTokens": {"state": "known", "value": "100"}
            }
        });
        conn.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES ('ins_r39i1b', 1, 'je1', ?1, '2026-10-01T00:00:00.000Z'),
                    ('ins_r39i1b', 2, 'je2', ?2, '2026-10-01T00:00:02.000Z')",
            params![journal_full.to_string(), journal_partial.to_string()],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let row: (i64, i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(metric_revision),
                        MAX(input_tokens), MAX(output_tokens),
                        MAX(cache_read_tokens), MAX(cache_write_tokens)
                 FROM usage_events WHERE instance_id='ins_r39i1b' AND scope='turn'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        let (count, revision, input, output, cache_read, cache_write) = row;
        assert_eq!(count, 1, "one turn survives");
        assert_eq!(revision, 101, "the newer revision is the winner");
        assert_eq!(input, Some(100));
        assert_eq!(
            output,
            Some(0),
            "the newer partial keeps the older known bucket"
        );
        assert_eq!(
            cache_read,
            Some(50),
            "known cache read survives the partial frame"
        );
        assert_eq!(cache_write, Some(0));
        // U1: the merged all-known basket derives the total the partial frame
        // did not report: 100 + 0 + 50 + 0 = 150.
        let total: Option<i64> = conn
            .query_row(
                "SELECT total_tokens FROM usage_events
                 WHERE instance_id='ins_r39i1b' AND scope='turn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            total,
            Some(150),
            "total derives from the merged known basket"
        );
    }

    #[test]
    fn recovered_cost_only_correction_keeps_counters_and_clock_like_live_upsert() {
        // --- Migration (recovery) path -------------------------------------
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        // Established row: seq1 rev100, native t0, input100, total150, the
        // remaining buckets Unknown.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 accounting, observed_at, observed_at_source)
             VALUES ('ins_r40i2', 1, 'pvp', 'm', 'turn', 'msg-T', 'snapshot',
                 100, 'sess-S', 150, 100,
                 'estimated', '2026-10-01T00:00:00.000Z', 'native')",
            [],
        )
        .unwrap();
        // Fallback append at seq2: cost-only correction, rev101, native t1.
        // Written through the 14-column legacy shape (no scope/sid columns).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, cost_usd,
                 accounting, observed_at)
             VALUES ('ins_r40i2', 2, 'pvp', 'm', 'turn', 'snapshot', '0.03',
                 'estimated', '2026-10-01T00:00:01.000Z')",
            [],
        )
        .unwrap();
        let journal = |seq: i64, revision: &str, cost: Option<&str>, stamp: &str| {
            let cost_json = match cost {
                Some(amount) => json!({
                    "state": "known",
                    "value": {"amount": amount, "currency": "USD"}
                }),
                None => json!({ "state": "unknown", "reason": "unpriced",
                                 "evidenceEventIds": [] }),
            };
            let mut payload = serde_json::Map::new();
            payload.insert("scope".into(), json!("turn"));
            payload.insert("scopeId".into(), json!("msg-T"));
            payload.insert("sourceSessionId".into(), json!("sess-S"));
            payload.insert("mode".into(), json!("snapshot"));
            payload.insert("metricRevision".into(), json!(revision));
            payload.insert("accounting".into(), json!("estimated"));
            payload.insert("cost".into(), cost_json);
            if seq == 1 {
                payload.insert("totalTokens".into(), json!({"state":"known","value":"150"}));
                payload.insert("inputTokens".into(), json!({"state":"known","value":"100"}));
            }
            json!({
                "kind": "usage",
                "nativeAt": {"state": "known", "value": stamp},
                "source": {
                    "nativeSessionId": {"state": "known", "value": "sess-S"},
                    "nativeTurnId": {"state": "known", "value": "msg-T"}
                },
                "payload": Value::Object(payload)
            })
        };
        let entries: [(i64, &str, Option<&str>, &str); 2] = [
            (1, "100", None, "2026-10-01T00:00:00.000Z"),
            (2, "101", Some("0.03"), "2026-10-01T00:00:01.000Z"),
        ];
        for (seq, rev, cost, stamp) in entries {
            let payload = journal(seq, rev, cost, stamp);
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES ('ins_r40i2', ?1, ?2, ?3, ?4)",
                params![seq, format!("je{seq}"), payload.to_string(), stamp],
            )
            .unwrap();
        }

        migrate(&conn).unwrap();

        #[derive(PartialEq, Debug)]
        struct SurvivingTurn {
            count: i64,
            seq: i64,
            revision: i64,
            scope: Option<String>,
            sid: Option<String>,
            total: Option<i64>,
            input: Option<i64>,
            output: Option<i64>,
            cache_read: Option<i64>,
            cache_write: Option<i64>,
            cost: Option<String>,
            observed_at: String,
            observed_at_source: String,
        }
        let read_survivor = |conn: &Connection| -> SurvivingTurn {
            conn.query_row(
                "SELECT COUNT(*) OVER (), seq, metric_revision, scope_id,
                        source_session_id, total_tokens, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, cost_usd,
                        observed_at, observed_at_source
                 FROM usage_events
                 WHERE instance_id = 'ins_r40i2' AND scope = 'turn'",
                [],
                |r| {
                    Ok(SurvivingTurn {
                        count: r.get(0)?,
                        seq: r.get(1)?,
                        revision: r.get(2)?,
                        scope: r.get(3)?,
                        sid: r.get(4)?,
                        total: r.get(5)?,
                        input: r.get(6)?,
                        output: r.get(7)?,
                        cache_read: r.get(8)?,
                        cache_write: r.get(9)?,
                        cost: r.get(10)?,
                        observed_at: r.get(11)?,
                        observed_at_source: r.get(12)?,
                    })
                },
            )
            .unwrap()
        };
        let recovered = read_survivor(&conn);
        assert_eq!(recovered.count, 1, "one reconciled turn row survives");
        assert_eq!(recovered.scope.as_deref(), Some("msg-T"));
        assert_eq!(recovered.sid.as_deref(), Some("sess-S"));
        assert_eq!(recovered.seq, 2, "the correction advances sequence");
        assert_eq!(recovered.revision, 101, "the correction advances revision");
        assert_eq!(recovered.cost.as_deref(), Some("0.03"), "bill corrected");
        assert_eq!(
            recovered.total,
            Some(150),
            "a cost-only correction keeps the known total it did not re-derive"
        );
        assert_eq!(recovered.input, Some(100), "known counters carry forward");
        assert_eq!(recovered.output, None, "still-Unknown buckets stay Unknown");
        assert_eq!(recovered.cache_read, None);
        assert_eq!(recovered.cache_write, None);
        assert_eq!(
            recovered.observed_at, "2026-10-01T00:00:00.000Z",
            "a billing correction never moves the native clock"
        );
        assert_eq!(recovered.observed_at_source, "native");

        // --- Live path ------------------------------------------------------
        let live = Connection::open_in_memory().unwrap();
        migrate(&live).unwrap();
        let frame = |seq: i64,
                     revision: i64,
                     total: Option<i64>,
                     input: Option<i64>,
                     cost: Option<&str>,
                     stamp: &str| UsageEventRow {
            instance_id: "ins_r40i2".into(),
            seq,
            profile_id: Some("pvp".into()),
            model: Some("m".into()),
            scope: "turn".into(),
            scope_id: Some("msg-T".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: revision,
            source_session_id: Some("sess-S".into()),
            total_tokens: total,
            input_tokens: input,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_usd: cost.map(str::to_string),
            accounting: "estimated".into(),
            observed_at: stamp.into(),
            observed_at_source: "native".into(),
        };
        assert!(
            insert_usage_event(
                &live,
                &frame(
                    1,
                    100,
                    Some(150),
                    Some(100),
                    None,
                    "2026-10-01T00:00:00.000Z"
                )
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &live,
                &frame(2, 101, None, None, Some("0.03"), "2026-10-01T00:00:01.000Z")
            )
            .unwrap()
        );
        let live_survivor = read_survivor(&live);
        assert_eq!(
            recovered, live_survivor,
            "recovery reconciliation must equal live reconciliation"
        );

        // --- Instance/supply parity on the recovered store ------------------
        let inst = aggregate_instance(&conn, "ins_r40i2").unwrap();
        let supply = aggregate_supply(&conn, "pvp", "m").unwrap();
        assert_eq!(inst.total_tokens, supply.total_tokens);
        assert_eq!(inst.input_tokens, supply.input_tokens);
        assert_eq!(inst.cost_usd, supply.cost_usd);
        assert_eq!(inst.total_tokens, Some(150));
        assert_eq!(inst.input_tokens, Some(100));
        assert!((inst.cost_usd.unwrap() - 0.03).abs() < 1e-9);
    }

    #[test]
    fn restart_keeps_the_later_reported_cumulative_bill() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r41i1",
                    1,
                    Some("s1"),
                    1,
                    None,
                    Some("0.01"),
                    "reported"
                )
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &r40_session_row(
                    "ins_r41i1",
                    2,
                    Some("s1"),
                    2,
                    None,
                    Some("0.03"),
                    "reported"
                )
            )
            .unwrap()
        );

        // Restart: every reconciliation step runs again.
        migrate(&conn).unwrap();

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = 'ins_r41i1' AND scope = 'session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "both growing bills survive restart");
        let newest: String = conn
            .query_row(
                "SELECT cost_usd FROM usage_events
                 WHERE instance_id = 'ins_r41i1' AND scope = 'session'
                 ORDER BY seq DESC, rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(newest, "0.03");

        let inst = aggregate_instance(&conn, "ins_r41i1").unwrap();
        let supply = aggregate_supply(&conn, "pvp_r40", "m-r40").unwrap();
        assert!(
            (inst.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "restart keeps $0.03"
        );
        assert_eq!(inst.cost_usd, supply.cost_usd, "instance/supply parity");
        assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Stop);
        assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Stop);
    }

    #[test]
    fn recovered_session_growth_with_a_decreasing_estimate_strips_cost_like_live() {
        // --- Recovery: r28 shape + fallback append + journal evidence --------
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        // Established scoped point: input100, estimate 0.10.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens,
                 cost_usd, accounting, observed_at, observed_at_source)
             VALUES ('ins_r41i2', 1, 'pvp', 'm', 'session', 'sess-S', 'snapshot',
                 1, 'sess-S', 100, 100, 0, 0, 0,
                 '0.10', 'estimated', '2026-10-01T00:00:00.000Z', 'native')",
            [],
        )
        .unwrap();
        // Fallback append (14-column shape): input200 with the RESET estimate
        // 0.05. Live acceptance keeps the growth and strips the cost.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, total_tokens,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 cost_usd, accounting, observed_at)
             VALUES ('ins_r41i2', 2, 'pvp', 'm', 'session', 'snapshot', 200,
                 200, 0, 0, 0, '0.05',
                 'estimated', '2026-10-01T00:00:02.000Z')",
            [],
        )
        .unwrap();
        let journal_point = |seq: i64, input: &str, cost: Option<&str>| {
            let cost_json = match cost {
                Some(amount) => json!({
                    "state": "known",
                    "value": {"amount": amount, "currency": "USD"}
                }),
                None => json!({ "state": "unknown", "reason": "unpriced",
                                 "evidenceEventIds": [] }),
            };
            json!({
                "kind": "usage",
                "nativeAt": {"state": "known",
                    "value": format!("2026-10-01T00:00:0{seq}.000Z")},
                "source": {
                    "nativeSessionId": {"state": "known", "value": "sess-S"}
                },
                "payload": {
                    "scope": "session",
                    "scopeId": "sess-S",
                    "mode": "snapshot",
                    "metricRevision": seq.to_string(),
                    "sourceSessionId": "sess-S",
                    "totalTokens": {"state": "known", "value": input},
                    "inputTokens": {"state": "known", "value": input},
                    "outputTokens": {"state": "known", "value": "0"},
                    "cacheReadTokens": {"state": "known", "value": "0"},
                    "cacheWriteTokens": {"state": "known", "value": "0"},
                    "cost": cost_json,
                    "accounting": "estimated"
                }
            })
        };
        for (seq, input, cost) in [(1i64, "100", Some("0.10")), (2, "200", Some("0.05"))] {
            let p = journal_point(seq, input, cost);
            let stamp = format!("2026-10-01T00:00:0{seq}.000Z");
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES ('ins_r41i2', ?1, ?2, ?3, ?4)",
                params![seq, format!("je{seq}"), p.to_string(), stamp],
            )
            .unwrap();
        }

        migrate(&conn).unwrap();

        // The newest retained point carries the growth but Unknown cost.
        let (input, cost): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT input_tokens, cost_usd FROM usage_events
                 WHERE instance_id = 'ins_r41i2' AND scope = 'session'
                 ORDER BY seq DESC, rowid DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(input, Some(200), "token growth is retained");
        assert_eq!(
            cost, None,
            "the reset estimate is stripped exactly like live"
        );

        // --- Live: project the same two observations through the live path ---
        let live = Connection::open_in_memory().unwrap();
        migrate(&live).unwrap();
        for seq in [1i64, 2] {
            let rec = JournalRecord {
                instance_id: "ins_r41i2".into(),
                seq,
                event_id: format!("evt{seq}"),
                event: serde_json::from_value(journal_point(
                    seq,
                    if seq == 1 { "100" } else { "200" },
                    if seq == 1 { Some("0.10") } else { Some("0.05") },
                ))
                .unwrap(),
                observed_at: format!("2026-10-01T00:00:0{seq}.000Z"),
            };
            let row = project_usage_event(&rec, Some("pvp"), Some("m")).unwrap();
            insert_usage_event(&live, &row).unwrap();
        }
        let live_point: (Option<i64>, Option<String>) = live
            .query_row(
                "SELECT input_tokens, cost_usd FROM usage_events
                 WHERE instance_id = 'ins_r41i2' AND scope = 'session'
                 ORDER BY seq DESC, rowid DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(live_point, (input, cost), "full retained point parity");

        // Aggregate / budget parity.
        let assert_unknown = |c: &Connection, label: &str| {
            let inst = aggregate_instance(c, "ins_r41i2").unwrap();
            let supply = aggregate_supply(c, "pvp", "m").unwrap();
            assert_eq!(inst.cost_usd, None, "{label}: recovered cost Unknown");
            assert_eq!(inst.cost_usd, supply.cost_usd, "{label}: parity");
            assert_eq!(inst.input_tokens, Some(200));
            assert_eq!(inst.input_tokens, supply.input_tokens);
            assert_eq!(inst.budget_status(Some(0.01)), BudgetStatus::Unknown);
            assert_eq!(supply.budget_status(Some(0.01)), BudgetStatus::Unknown);
        };
        assert_unknown(&conn, "recovery");
        assert_unknown(&live, "live");
    }

    #[test]
    fn turn_reconciliation_updates_and_preserves_native_context_window() {
        let frame = |conn_inst: &str,
                     seq: i64,
                     revision: i64,
                     input: Option<i64>,
                     cost: Option<&str>,
                     window: Option<i64>| UsageEventRow {
            instance_id: conn_inst.into(),
            seq,
            profile_id: Some("pvp".into()),
            model: Some("m".into()),
            scope: "turn".into(),
            scope_id: Some("msg-T".into()),
            turn_id: None,
            native_context_window: window,
            mode: "snapshot".into(),
            metric_revision: revision,
            source_session_id: Some("sess-S".into()),
            total_tokens: input,
            input_tokens: input,
            output_tokens: input.map(|_| 0),
            cache_read_tokens: input.map(|_| 0),
            cache_write_tokens: input.map(|_| 0),
            cost_usd: cost.map(str::to_string),
            accounting: "estimated".into(),
            observed_at: format!("2026-10-01T00:00:0{seq}.000Z"),
            observed_at_source: "native".into(),
        };

        // --- Live -------------------------------------------------------------
        let live = Connection::open_in_memory().unwrap();
        migrate(&live).unwrap();
        assert!(
            insert_usage_event(
                &live,
                &frame("ins_r41i3", 1, 1, Some(100), None, Some(200_000))
            )
            .unwrap()
        );
        // Newer counter snapshot reports a 1M context window.
        assert!(
            insert_usage_event(
                &live,
                &frame("ins_r41i3", 2, 2, Some(500_000), None, Some(1_000_000))
            )
            .unwrap()
        );
        let after_counters: (i64, Option<i64>) = live
            .query_row(
                "SELECT metric_revision, native_context_window FROM usage_events
                 WHERE instance_id = 'ins_r41i3' AND scope = 'turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            after_counters,
            (2, Some(1_000_000)),
            "newer snapshot updates the window"
        );
        // A strictly-newer cost-only correction must keep window and counters.
        assert!(
            insert_usage_event(&live, &frame("ins_r41i3", 3, 3, None, Some("0.03"), None)).unwrap()
        );
        let live_survivor: (i64, i64, Option<String>, Option<i64>, Option<i64>) = live
            .query_row(
                "SELECT metric_revision, input_tokens, cost_usd, native_context_window,
                        total_tokens FROM usage_events
                 WHERE instance_id = 'ins_r41i3' AND scope = 'turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            live_survivor,
            (
                3,
                500_000,
                Some("0.03".into()),
                Some(1_000_000),
                Some(500_000)
            ),
            "cost-only correction keeps the retained 1M window and counters"
        );

        // --- Recovery: established 1M turn then a fallback cost-only append --
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at, observed_at_source)
             VALUES ('ins_r41i3', 2, 'pvp', 'm', 'turn', 'msg-T', 'snapshot',
                 2, 'sess-S', 500000, 500000, 0, 0, 0,
                 'estimated', '2026-10-01T00:00:02.000Z', 'native')",
            [],
        )
        .unwrap();
        // The established 1M window is absent from the r28 shape and is
        // re-projected from the journal (contextWindow) like every fallback
        // field; the cost-only seq3 row carries no window anywhere.
        // Fallback cost-only append at seq3 (14 columns: no window/scope).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, cost_usd,
                 accounting, observed_at)
             VALUES ('ins_r41i3', 3, 'pvp', 'm', 'turn', 'snapshot', '0.03',
                 'estimated', '2026-10-01T00:00:03.000Z')",
            [],
        )
        .unwrap();
        let journal_turn = |seq: i64, revision: &str, window: Option<i64>| {
            let mut payload = serde_json::Map::new();
            payload.insert("scope".into(), json!("turn"));
            payload.insert("scopeId".into(), json!("msg-T"));
            payload.insert("sourceSessionId".into(), json!("sess-S"));
            payload.insert("mode".into(), json!("snapshot"));
            payload.insert("metricRevision".into(), json!(revision));
            payload.insert("accounting".into(), json!("estimated"));
            payload.insert(
                "cost".into(),
                json!({
                    "state": "known",
                    "value": {"amount": "0.03", "currency": "USD"}
                }),
            );
            if seq == 2 {
                payload.insert(
                    "totalTokens".into(),
                    json!({"state":"known","value":"500000"}),
                );
                payload.insert(
                    "inputTokens".into(),
                    json!({"state":"known","value":"500000"}),
                );
                payload.insert("outputTokens".into(), json!({"state":"known","value":"0"}));
                payload.insert(
                    "cacheReadTokens".into(),
                    json!({"state":"known","value":"0"}),
                );
                payload.insert(
                    "cacheWriteTokens".into(),
                    json!({"state":"known","value":"0"}),
                );
            }
            if let Some(w) = window {
                payload.insert("contextWindow".into(), json!(w));
            }
            json!({
                "kind": "usage",
                "nativeAt": {"state": "known",
                    "value": format!("2026-10-01T00:00:0{seq}.000Z")},
                "source": {
                    "nativeSessionId": {"state": "known", "value": "sess-S"},
                    "nativeTurnId": {"state": "known", "value": "msg-T"}
                },
                "payload": Value::Object(payload)
            })
        };
        for (seq, rev, window) in [(2i64, "2", Some(1_000_000i64)), (3, "3", None)] {
            let p = journal_turn(seq, rev, window);
            let stamp = format!("2026-10-01T00:00:0{seq}.000Z");
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES ('ins_r41i3', ?1, ?2, ?3, ?4)",
                params![seq, format!("je{seq}"), p.to_string(), stamp],
            )
            .unwrap();
        }
        migrate(&conn).unwrap();

        let recovered_survivor: (i64, i64, Option<String>, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT metric_revision, input_tokens, cost_usd, native_context_window,
                        total_tokens FROM usage_events
                 WHERE instance_id = 'ins_r41i3' AND scope = 'turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            recovered_survivor,
            (
                3,
                500_000,
                Some("0.03".into()),
                Some(1_000_000),
                Some(500_000)
            ),
            "recovery cost-only correction keeps the retained 1M window"
        );
        // Compare the FULL common row shape (drop seq/rowid/clock: the
        // established frame is seq2 here vs a seq1 insert on the live path).
        let common = |c: &Connection| {
            c.query_row(
                "SELECT metric_revision, source_session_id, scope_id, total_tokens,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        cost_usd, accounting, native_context_window
                 FROM usage_events
                 WHERE instance_id = 'ins_r41i3' AND scope = 'turn'",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, String>(9)?,
                        r.get::<_, Option<i64>>(10)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(
            common(&live),
            common(&conn),
            "FULL recovered/live row parity"
        );

        // Rollup resolves the 1M window; 500_000 used is exactly 50%.
        for (label, c) in [("live", &live), ("recovery", &conn)] {
            let rollup = rollup_instance(
                c,
                &RollupRequest {
                    instance_id: "ins_r41i3",
                    kind: "claude",
                    spec_model: Some("m"),
                    effective_model: None,
                    profile_id: Some("pvp"),
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                rollup.context_window_tokens,
                Some(1_000_000),
                "{label} window stays 1M after the cost-only correction"
            );
            assert_eq!(
                rollup.context_pct,
                Some(50),
                "{label} percentage follows 1M"
            );
            assert_eq!(rollup.context_used_tokens, Some(500_000));
            let inst = aggregate_instance(c, "ins_r41i3").unwrap();
            let supply = aggregate_supply(c, "pvp", "m").unwrap();
            assert!((inst.cost_usd.unwrap() - 0.03).abs() < 1e-9);
            assert_eq!(inst.cost_usd, supply.cost_usd, "{label} cost parity");
            assert_eq!(inst.input_tokens, supply.input_tokens);
            assert_eq!(inst.budget_status(Some(0.02)), BudgetStatus::Stop);
            assert_eq!(supply.budget_status(Some(0.02)), BudgetStatus::Stop);
        }
    }

    fn r42_full_turn_row(conn: &Connection, instance: &str) -> Vec<String> {
        conn.prepare(
            "SELECT metric_revision, profile_id, model, mode, scope_id, turn_id,
                    scope, total_tokens, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                    observed_at, observed_at_source, native_context_window,
                    COALESCE(lineage_state,''), source_session_id
             FROM usage_events
             WHERE instance_id = ?1 AND scope = 'turn'",
        )
        .unwrap()
        .query_row(params![instance], |r| {
            (0..19)
                .map(|i| {
                    let v = r.get_ref(i)?;
                    Ok::<_, rusqlite::Error>(match v {
                        rusqlite::types::ValueRef::Null => "<NULL>".to_string(),
                        rusqlite::types::ValueRef::Integer(i) => i.to_string(),
                        rusqlite::types::ValueRef::Real(f) => f.to_string(),
                        other => other.as_str().unwrap_or("<blob>").to_string(),
                    })
                })
                .collect()
        })
        .unwrap()
    }

    #[test]
    fn restart_reconstruction_never_installs_a_cost_only_corrections_window_or_clock() {
        let journal_payload = |seq: i64, revision: &str, kind: &str| {
            let mut payload = serde_json::Map::new();
            payload.insert("scope".into(), json!("turn"));
            payload.insert("scopeId".into(), json!("msg-T"));
            payload.insert("sourceSessionId".into(), json!("sess-S"));
            payload.insert("mode".into(), json!("snapshot"));
            payload.insert("metricRevision".into(), json!(revision));
            payload.insert("accounting".into(), json!("estimated"));
            if kind == "counters" {
                payload.insert(
                    "totalTokens".into(),
                    json!({"state":"known","value":"500000"}),
                );
                payload.insert(
                    "inputTokens".into(),
                    json!({"state":"known","value":"500000"}),
                );
                payload.insert("outputTokens".into(), json!({"state":"known","value":"0"}));
                payload.insert(
                    "cacheReadTokens".into(),
                    json!({"state":"known","value":"0"}),
                );
                payload.insert(
                    "cacheWriteTokens".into(),
                    json!({"state":"known","value":"0"}),
                );
            } else {
                // Cost-only correction: bill, window and native time but NO
                // token baskets.
                payload.insert("contextWindow".into(), json!(1_000_000));
                payload.insert(
                    "cost".into(),
                    json!({
                        "state":"known",
                        "value":{"amount":"0.03","currency":"USD"}
                    }),
                );
            }
            let mut event = serde_json::Map::new();
            event.insert("kind".into(), json!("usage"));
            event.insert("payload".into(), Value::Object(payload));
            event.insert(
                "source".into(),
                json!({
                    "nativeSessionId": {"state":"known","value":"sess-S"},
                    "nativeTurnId": {"state":"known","value":"msg-T"}
                }),
            );
            // Only the counter frame carries a native time — and this case
            // deliberately gives it none, so its stored clock is ingest.
            if seq == 2 {
                event.insert(
                    "nativeAt".into(),
                    json!({"state":"known","value":"2026-10-18T00:00:01.000Z"}),
                );
            }
            Value::Object(event)
        };

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        for (seq, rev, kind, stamp) in [
            (1i64, "1", "counters", "2026-10-18T00:00:00.000Z"),
            (2, "2", "costonly", "2026-10-18T00:00:01.000Z"),
        ] {
            let event = journal_payload(seq, rev, kind);
            let record = JournalRecord {
                instance_id: "ins_r42i2a".into(),
                seq,
                event_id: format!("je{seq}"),
                event: event.clone(),
                observed_at: stamp.into(),
            };
            insert_journal_raw(&conn, &record);
            let row = project_usage_event(&record, Some("pvp"), Some("m")).unwrap();
            insert_usage_event(&conn, &row).unwrap();
        }

        // Live reconciliation: missing window and ingest clock are retained.
        let before = r42_full_turn_row(&conn, "ins_r42i2a");
        assert_eq!(before[16], "<NULL>", "live keeps the retained NULL window");
        assert_eq!(before[15], "ingest", "live keeps the retained ingest clock");
        assert_eq!(before[14], "2026-10-18T00:00:00.000Z");

        // Restart: reconstruction runs again against the single reconciled
        // row (whose seq now points at the correction's journal entry).
        migrate(&conn).unwrap();
        let after = r42_full_turn_row(&conn, "ins_r42i2a");
        assert_eq!(
            before, after,
            "restart changes nothing the live reconciliation deliberately kept"
        );

        // The rollup does NOT fabricate a 1M native window.
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r42i2a",
                kind: "codex",
                spec_model: Some("totally-unknown-model"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        // The rollup falls back to the codex KIND window (200k) — the
        // correction's 1M native window never leaks in.
        assert_eq!(
            rollup.context_window_tokens,
            Some(200_000),
            "no leaked native 1M window after restart"
        );
        assert_eq!(
            rollup.context_used_tokens,
            Some(500_000),
            "the counter frame's context basket survives"
        );
    }

    #[test]
    fn fallback_recovery_twice_keeps_the_retained_window_and_clock_idempotent() {
        let build = || -> Connection {
            let conn = Connection::open_in_memory().unwrap();
            create_r28_usage_table_with_turn_index(&conn);
            create_journal_table(&conn);
            // Established fallback row: counters, no window column on r28,
            // ingest clock t0.
            conn.execute(
                "INSERT INTO usage_events
                    (instance_id, seq, profile_id, model, scope, scope_id, mode,
                     metric_revision, source_session_id, total_tokens, input_tokens,
                     output_tokens, cache_read_tokens, cache_write_tokens,
                     accounting, observed_at, observed_at_source)
                 VALUES ('ins_r42i2b', 1, 'pvp', 'm', 'turn', 'msg-T', 'snapshot',
                     1, 'sess-S', 500000, 500000, 0, 0, 0,
                     'estimated', '2026-10-18T00:00:00.000Z', 'ingest')",
                [],
            )
            .unwrap();
            // Cost-only fallback append at seq2 (14-column shape).
            conn.execute(
                "INSERT INTO usage_events
                    (instance_id, seq, profile_id, model, scope, mode, cost_usd,
                     accounting, observed_at)
                 VALUES ('ins_r42i2b', 2, 'pvp', 'm', 'turn', 'snapshot', '0.03',
                     'estimated', '2026-10-18T00:00:01.000Z')",
                [],
            )
            .unwrap();
            let journal = |seq: i64, kind: &str| {
                let mut payload = serde_json::Map::new();
                payload.insert("scope".into(), json!("turn"));
                payload.insert("scopeId".into(), json!("msg-T"));
                payload.insert("sourceSessionId".into(), json!("sess-S"));
                payload.insert("mode".into(), json!("snapshot"));
                payload.insert("metricRevision".into(), json!(seq.to_string()));
                payload.insert("accounting".into(), json!("estimated"));
                if kind == "counters" {
                    payload.insert(
                        "totalTokens".into(),
                        json!({"state":"known","value":"500000"}),
                    );
                    payload.insert(
                        "inputTokens".into(),
                        json!({"state":"known","value":"500000"}),
                    );
                    payload.insert("outputTokens".into(), json!({"state":"known","value":"0"}));
                    payload.insert(
                        "cacheReadTokens".into(),
                        json!({"state":"known","value":"0"}),
                    );
                    payload.insert(
                        "cacheWriteTokens".into(),
                        json!({"state":"known","value":"0"}),
                    );
                } else {
                    payload.insert("contextWindow".into(), json!(1_000_000));
                    payload.insert(
                        "cost".into(),
                        json!({
                            "state":"known",
                            "value":{"amount":"0.03","currency":"USD"}
                        }),
                    );
                }
                let mut event = serde_json::Map::new();
                event.insert("kind".into(), json!("usage"));
                event.insert("payload".into(), Value::Object(payload));
                event.insert(
                    "source".into(),
                    json!({
                        "nativeSessionId": {"state":"known","value":"sess-S"},
                        "nativeTurnId": {"state":"known","value":"msg-T"}
                    }),
                );
                if seq == 2 {
                    event.insert(
                        "nativeAt".into(),
                        json!({"state":"known","value":"2026-10-18T00:00:01.000Z"}),
                    );
                }
                Value::Object(event)
            };
            for (seq, kind, stamp) in [
                (1i64, "counters", "2026-10-18T00:00:00.000Z"),
                (2, "costonly", "2026-10-18T00:00:01.000Z"),
            ] {
                let p = journal(seq, kind);
                conn.execute(
                    "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                     VALUES ('ins_r42i2b', ?1, ?2, ?3, ?4)",
                    params![seq, format!("je{seq}"), p.to_string(), stamp],
                )
                .unwrap();
            }
            conn
        };

        let conn = build();
        migrate(&conn).unwrap();
        let once = r42_full_turn_row(&conn, "ins_r42i2b");
        assert_eq!(
            once[16], "<NULL>",
            "first migration keeps the retained NULL window"
        );
        assert_eq!(once[15], "ingest", "first migration keeps the ingest clock");
        assert_eq!(once[14], "2026-10-18T00:00:00.000Z");
        assert_eq!(once[12], "0.03", "the correction bill survives");
        assert_eq!(once[8], "500000", "counters carry forward");

        // Second migration must be idempotent — no window/clock leak from the
        // surviving correction's journal entry.
        migrate(&conn).unwrap();
        let twice = r42_full_turn_row(&conn, "ins_r42i2b");
        assert_eq!(once, twice, "migration twice is idempotent");

        let inst = aggregate_instance(&conn, "ins_r42i2b").unwrap();
        let supply = aggregate_supply(&conn, "pvp", "m").unwrap();
        assert!((inst.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(inst.cost_usd, supply.cost_usd);
        assert_eq!(inst.input_tokens, Some(500_000));
        assert_eq!(inst.input_tokens, supply.input_tokens);
    }

    fn r43_real_usage_journal(
        instance: &str,
        seq: i64,
        revision: u64,
        counter: bool,
        cost: Option<&str>,
        window: Option<u64>,
        native_at: Option<&str>,
    ) -> JournalRecord {
        use remuda_protocol::{
            Accounting, Cost, Id, InputAccounting, Knowledge, U64, UsageMode, UsagePayload,
            UsageScope,
        };
        let unknown = || Knowledge::Unknown {
            reason: "unpriced".into(),
            evidence_event_ids: vec![],
        };
        let known_u64 = |v: u64| Knowledge::Known { value: U64(v) };
        // A counter frame reports Known 500000/0/0/0; a billing correction
        // serialises every additive basket as explicit Unknown.
        let (input, output, cache_read, cache_write, total) = if counter {
            (
                known_u64(500_000),
                known_u64(0),
                known_u64(0),
                known_u64(0),
                known_u64(500_000),
            )
        } else {
            (unknown(), unknown(), unknown(), unknown(), unknown())
        };
        let cost: Knowledge<Cost> = match cost {
            Some(amount) => Knowledge::Known {
                value: Cost {
                    amount: amount.into(),
                    currency: "USD".into(),
                },
            },
            None => Knowledge::Unknown {
                reason: "unpriced".into(),
                evidence_event_ids: vec![],
            },
        };
        let payload = UsagePayload {
            usage_id: Id::new("obj").unwrap(),
            scope: UsageScope::Turn,
            scope_id: "msg-T".into(),
            mode: UsageMode::Snapshot,
            metric_revision: U64(revision),
            source_session_id: Some("sess-S".into()),
            input_tokens: input,
            input_accounting: InputAccounting::Uncached,
            output_tokens: output,
            reasoning_tokens: unknown(),
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            total_tokens: total,
            cost,
            accounting: Accounting::Estimated,
            native_fields_ref: None,
            context_window: window.map(U64),
        };
        let mut event = serde_json::Map::new();
        event.insert("kind".into(), json!("usage"));
        event.insert("payload".into(), serde_json::to_value(payload).unwrap());
        event.insert(
            "source".into(),
            json!({
                "nativeSessionId": {"state":"known","value":"sess-S"},
                "nativeTurnId": {"state":"known","value":"msg-T"}
            }),
        );
        if let Some(at) = native_at {
            event.insert("nativeAt".into(), json!({"state":"known","value":at}));
        }
        let stamp = native_at.unwrap_or("2026-10-19T00:00:00.000Z");
        JournalRecord {
            instance_id: instance.into(),
            seq,
            event_id: format!("je{seq}"),
            event: Value::Object(event),
            observed_at: stamp.into(),
        }
    }

    #[test]
    fn restart_with_real_unknown_baskets_keeps_the_retained_window_and_clock() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        let counter = r43_real_usage_journal("ins_r43i1", 1, 1, true, None, None, None);
        let correction = r43_real_usage_journal(
            "ins_r43i1",
            2,
            2,
            false,
            Some("0.03"),
            Some(1_000_000),
            Some("2026-10-19T00:00:01.000Z"),
        );
        for record in [&counter, &correction] {
            insert_journal_raw(&conn, record);
            let row = project_usage_event(record, Some("pvp"), Some("m")).unwrap();
            insert_usage_event(&conn, &row).unwrap();
        }

        // Live reconciliation: Unknown baskets == no counters -> retained
        // NULL window and ingest clock survive.
        let live = r42_full_turn_row(&conn, "ins_r43i1");
        assert_eq!(live[16], "<NULL>", "live keeps the retained NULL window");
        assert_eq!(live[15], "ingest");
        assert_eq!(live[12], "0.03");

        // Restart must not let the correction's Unknown baskets count.
        migrate(&conn).unwrap();
        assert_eq!(
            r42_full_turn_row(&conn, "ins_r43i1"),
            live,
            "restart == live with explicit Knowledge::Unknown baskets"
        );
    }

    #[test]
    fn fallback_twice_with_real_unknown_baskets_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        // Established fallback counter row: no window, ingest clock.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at, observed_at_source)
             VALUES ('ins_r43i1b', 1, 'pvp', 'm', 'turn', 'msg-T', 'snapshot',
                 1, 'sess-S', 500000, 500000, 0, 0, 0,
                 'estimated', '2026-10-19T00:00:00.000Z', 'ingest')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, cost_usd,
                 accounting, observed_at)
             VALUES ('ins_r43i1b', 2, 'pvp', 'm', 'turn', 'snapshot', '0.03',
                 'estimated', '2026-10-19T00:00:01.000Z')",
            [],
        )
        .unwrap();
        for record in [
            r43_real_usage_journal("ins_r43i1b", 1, 1, true, None, None, None),
            r43_real_usage_journal(
                "ins_r43i1b",
                2,
                2,
                false,
                Some("0.03"),
                Some(1_000_000),
                Some("2026-10-19T00:00:01.000Z"),
            ),
        ] {
            insert_journal_raw(&conn, &record);
        }

        migrate(&conn).unwrap();
        let once = r42_full_turn_row(&conn, "ins_r43i1b");
        assert_eq!(once[16], "<NULL>", "first migration keeps NULL window");
        assert_eq!(once[15], "ingest");
        migrate(&conn).unwrap();
        assert_eq!(
            r42_full_turn_row(&conn, "ins_r43i1b"),
            once,
            "second migration with Unknown baskets is idempotent"
        );
    }

    #[test]
    fn an_upgraded_ingest_only_replay_is_not_promoted_to_a_live_increment() {
        let create_r28 = "CREATE TABLE usage_events (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            profile_id TEXT,
            model TEXT,
            scope TEXT NOT NULL,
            scope_id TEXT,
            mode TEXT NOT NULL,
            total_tokens INTEGER,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            cost_usd TEXT,
            accounting TEXT NOT NULL DEFAULT 'estimated',
            observed_at TEXT NOT NULL,
            observed_at_source TEXT NOT NULL DEFAULT 'ingest',
            metric_revision INTEGER NOT NULL DEFAULT 1,
            source_session_id TEXT,
            PRIMARY KEY (instance_id, seq)
         );";
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(create_r28).unwrap();
        let native_stamp = threshold_rfc3339(20);
        let ingest_stamp = threshold_rfc3339(2);
        // A native 100/60 stock, then a timestamp-free historical replay that
        // carries 160/84 and is stored with its INGEST time and 'ingest'
        // source — exactly the r28 live-path invariant.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, mode,
                 input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, accounting, observed_at,
                 observed_at_source, metric_revision, source_session_id)
             VALUES
                ('ins_r37i3', 1, 'session', 'g1', 'snapshot',
                 100, 60, 0, 0, 'estimated', ?1, 'native', 1, 'g1'),
                ('ins_r37i3', 2, 'session', 'g1', 'snapshot',
                 160, 84, 0, 0, 'estimated', ?2, 'ingest', 2, 'g1')",
            params![native_stamp, ingest_stamp],
        )
        .unwrap();

        migrate(&conn).unwrap();

        // The tag never rewrites the explicit provenance.
        let sources: Vec<String> = conn
            .prepare(
                "SELECT observed_at_source FROM usage_events
                 WHERE instance_id='ins_r37i3' ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(sources, vec!["native", "ingest"]);

        let rollup = rollup_fold(&conn, "ins_r37i3", "grok", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(160),
            "the replayed cumulative stock still reports the retained total"
        );
        assert_eq!(
            rollup.tpm_in_60s, None,
            "an ingest-only replay is not a native 60-token increment"
        );
        assert_eq!(rollup.tpm_out_60s, None);
        assert_eq!(rollup.tpm_in_5m, None);
        assert_eq!(rollup.tpm_out_5m, None);
    }

    #[test]
    fn message_coverage_is_measured_against_the_winning_lineage_turn() {
        use remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX;
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let at = "2026-10-13T00:00:00.000Z";
        let turn = |seq: i64, lineage: &str, input: i64, output: i64| UsageEventRow {
            instance_id: "ins_r37i5".into(),
            seq,
            profile_id: Some("pvp".into()),
            model: Some("m".into()),
            scope: "turn".into(),
            scope_id: Some("msg-T".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: Some(lineage.into()),
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: at.into(),
            observed_at_source: "native".into(),
        };
        // Loser first (seq 1, 100/10), winner second (seq 2, 200/20).
        assert!(insert_usage_event(&conn, &turn(1, "sess-a", 100, 10)).unwrap());
        assert!(insert_usage_event(&conn, &turn(2, "sess-b", 200, 20)).unwrap());
        // A DURABLE Message linked to the turn whose buckets match the LOSER
        // (100/10), never the 200/20 winner.
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r37i5".into(),
                    seq: 3,
                    profile_id: Some("pvp".into()),
                    model: Some("m".into()),
                    scope: "message".into(),
                    scope_id: Some(format!("{CODEX_DURABLE_RESPONSE_SCOPE_PREFIX}0:file-1")),
                    turn_id: Some("msg-T".into()),
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: None,
                    total_tokens: Some(110),
                    input_tokens: Some(100),
                    output_tokens: Some(10),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                    accounting: "estimated".into(),
                    observed_at: at.into(),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r37i5",
                kind: "codex",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(200),
            "the winner's 200 bills; matching the losing lineage's 100 must \
             never mark the winner covered"
        );
        assert_eq!(rollup.session_output_tokens, Some(20));
    }

    #[test]
    fn another_instances_native_turn_does_not_force_the_turn_rate_route() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let session_point = |seq: i64, input: i64, stamp: String| UsageEventRow {
            instance_id: "ins_r37i6a".into(),
            seq,
            profile_id: Some("pvp".into()),
            model: Some("m".into()),
            scope: "session".into(),
            scope_id: Some("sess-a".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: seq,
            source_session_id: Some("sess-a".into()),
            total_tokens: Some(input),
            input_tokens: Some(input),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: stamp,
            observed_at_source: "native".into(),
        };
        // A: two native stocks, 100 -> 150, the increment inside 60 s.
        assert!(insert_usage_event(&conn, &session_point(1, 100, ago(20))).unwrap());
        assert!(insert_usage_event(&conn, &session_point(2, 150, ago(2))).unwrap());
        // B: a native, uncovered winning Turn (a DIFFERENT instance).
        assert!(
            insert_usage_event(
                &conn,
                &UsageEventRow {
                    instance_id: "ins_r37i6b".into(),
                    seq: 1,
                    profile_id: Some("pvp".into()),
                    model: Some("m".into()),
                    scope: "turn".into(),
                    scope_id: Some("msg-b".into()),
                    turn_id: None,
                    native_context_window: None,
                    mode: "snapshot".into(),
                    metric_revision: 1,
                    source_session_id: Some("sess-b".into()),
                    total_tokens: Some(999),
                    input_tokens: Some(900),
                    output_tokens: Some(99),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                    accounting: "estimated".into(),
                    observed_at: ago(1),
                    observed_at_source: "native".into(),
                }
            )
            .unwrap()
        );

        let rollup_a = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r37i6a",
                kind: "grok",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup_a.tpm_in_60s,
            Some(50),
            "A keeps the stock-increment route despite B's native turn"
        );

        // B itself DOES take the turn-rate route.
        let rollup_b = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r37i6b",
                kind: "claude",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup_b.tpm_in_60s,
            Some(900),
            "B's own turn drives B's rate"
        );
    }

    #[test]
    fn reopen_never_lowers_a_durable_message_revision_then_a_stale_replay_loses() {
        let marker = remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX;
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        // r46: construct a PREVIOUS-RELEASE current-schema upgrade fixture —
        // the replay marker must NOT already be installed, or the reopen below
        // skips replay and this test proves nothing.
        conn.execute(
            "DELETE FROM usage_schema_meta WHERE key = 'journal_replay_v1'",
            [],
        )
        .unwrap();

        // Journal retains BOTH frames at their original sequences.
        let journal_usage = |_seq: i64, revision: i64, input: i64| {
            json!({
                "kind": "usage",
                "nativeAt": {"state": "known", "value": "2026-10-13T00:00:00.000Z"},
                "payload": {
                    "scope": "message",
                    "scopeId": format!("{marker}0:file-M"),
                    "mode": "snapshot",
                    "metricRevision": revision.to_string(),
                    "accounting": "estimated",
                    "inputTokens": {"state": "known", "value": input.to_string()},
                    "outputTokens": {"state": "known", "value": "0"},
                    "cacheReadTokens": {"state": "known", "value": "0"},
                    "cacheWriteTokens": {"state": "known", "value": "0"},
                    "totalTokens": {"state": "known", "value": input.to_string()}
                }
            })
        };
        for (seq, rev, input) in [(1, 1, 100), (2, 2, 150)] {
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES ('ins_r38i1', ?1, ?2, ?3, '2026-10-13T00:00:00.000Z')",
                params![
                    seq,
                    format!("je{seq}"),
                    journal_usage(seq, rev, input).to_string()
                ],
            )
            .unwrap();
        }

        // Simulate the live upsert: ONE message row kept at the ORIGINAL seq 1
        // but carrying the revision-2 final counters (exactly what
        // upsert_message_row leaves behind).
        let row = UsageEventRow {
            instance_id: "ins_r38i1".into(),
            seq: 1,
            profile_id: None,
            model: None,
            scope: "message".into(),
            scope_id: Some(format!("{marker}0:file-M")),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 2,
            source_session_id: None,
            total_tokens: Some(150),
            input_tokens: Some(150),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: "2026-10-13T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        };
        assert!(insert_usage_event(&conn, &row).unwrap());

        // Reopen: the migration runs again against the retained journal.
        migrate(&conn).unwrap();

        let (revision, input): (i64, Option<i64>) = conn
            .query_row(
                "SELECT metric_revision, input_tokens FROM usage_events
                 WHERE instance_id='ins_r38i1' AND scope='message'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            revision, 2,
            "reopen must not lower the revision watermark to 1"
        );
        assert_eq!(
            input,
            Some(150),
            "reopen must not revert the rev-2 counters"
        );

        // A stale revision-1 replay delivered after reopen still loses to the
        // established revision-2 authority (counters stay 150).
        let stale = JournalRecord {
            instance_id: "ins_r38i1".into(),
            seq: 3,
            event_id: "evt_stale".into(),
            event: journal_usage(3, 1, 100),
            observed_at: "2026-10-13T00:00:01.000Z".into(),
        };
        let stale_row = project_usage_event(&stale, None, None).unwrap();
        assert!(
            !insert_usage_event(&conn, &stale_row).unwrap(),
            "a revision-1 replay cannot replace the established revision 2"
        );
        let input_after: Option<i64> = conn
            .query_row(
                "SELECT input_tokens FROM usage_events
                 WHERE instance_id='ins_r38i1' AND scope='message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(input_after, Some(150));
    }

    #[test]
    fn a_not_applicable_cache_bucket_is_a_structural_zero_for_context() {
        let wire = |cache_write: Value| {
            json!({
                "schemaVersion": 1,
                "eventId": "evt_na",
                "journalId": "obj_na",
                "instanceId": "ins_r37na",
                "hostId": "hst",
                "processGeneration": "1",
                "seq": "1",
                "observedAt": "2026-10-09T10:00:00.000Z",
                "kind": "usage",
                "completeness": "structured",
                "payload": {
                    "usageId": "obj_na-turn",
                    "scope": "turn",
                    "scopeId": "msg_na",
                    "mode": "snapshot",
                    "metricRevision": "1",
                    "inputAccounting": "uncached",
                    "accounting": "estimated",
                    "inputTokens": {"state": "known", "value": "100000"},
                    "outputTokens": {"state": "known", "value": "1000"},
                    "reasoningTokens": {"state": "known", "value": "0"},
                    "cacheReadTokens": {"state": "known", "value": "0"},
                    "cacheWriteTokens": cache_write,
                    "totalTokens": {"state": "known", "value": "101000"}
                }
            })
        };
        let project = |event: Value| {
            let rec = crate::store::JournalRecord {
                instance_id: "ins_r37na".into(),
                seq: 1,
                event_id: "evt_na".into(),
                event,
                observed_at: "2026-10-09T10:00:00.000Z".into(),
            };
            project_usage_event(&rec, Some("pvp"), Some("claude-haiku-4-5")).unwrap()
        };
        let rollup_for = |row: &UsageEventRow| {
            let conn = Connection::open_in_memory().unwrap();
            migrate(&conn).unwrap();
            assert!(insert_usage_event(&conn, row).unwrap());
            rollup_instance(
                &conn,
                &RollupRequest {
                    instance_id: "ins_r37na",
                    kind: "claude",
                    spec_model: Some("claude-haiku-4-5"),
                    effective_model: None,
                    profile_id: Some("pvp"),
                },
            )
            .unwrap()
            .unwrap()
        };

        // not-applicable projects to a stored zero and the context basket is
        // known: 100,000 + 0 + 0.
        let na_row = project(wire(json!({"state": "not-applicable"})));
        assert_eq!(na_row.cache_write_tokens, Some(0));
        let na_rollup = rollup_for(&na_row);
        assert_eq!(na_rollup.context_used_tokens, Some(100_000));
        assert_eq!(na_rollup.context_pct, Some(50));

        // The same frame with the bucket genuinely OMITTED stays Unknown: U1
        // never invents a zero for an unreported bucket.
        let mut omitted_payload = wire(json!({"state": "unknown", "reason": "omitted"}));
        omitted_payload["payload"]
            .as_object_mut()
            .unwrap()
            .remove("cacheWriteTokens");
        let omitted_row = project(omitted_payload);
        assert_eq!(omitted_row.cache_write_tokens, None);
        let omitted_rollup = rollup_for(&omitted_row);
        assert_eq!(
            omitted_rollup.context_used_tokens, None,
            "an unreported bucket keeps the context Unknown"
        );
    }

    #[test]
    fn an_unqualified_frame_never_rewrites_a_qualified_rows_identity() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(insert(
            &conn,
            &qualified_record(1, "ins_t1p_b", "msg-2", "sess-alpha", 3, 100, 30)
        ));
        assert!(
            insert(
                &conn,
                &scoped_record(
                    2,
                    "ins_t1p_b",
                    "turn",
                    Some("msg-2"),
                    1,
                    40,
                    4,
                    0,
                    0,
                    Some("2026-10-10T00:00:00.000Z")
                )
            ),
            "the unqualified lineage is independent of the qualified row"
        );
        let identity: (Option<String>, i64) = conn
            .query_row(
                "SELECT source_session_id, metric_revision FROM usage_events
                 WHERE instance_id='ins_t1p_b' AND scope_id='msg-2'
                   AND source_session_id = 'sess-alpha'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(identity, (Some("sess-alpha".to_string()), 3));
        let null_identity: (Option<String>, i64) = conn
            .query_row(
                "SELECT source_session_id, metric_revision FROM usage_events
                 WHERE instance_id='ins_t1p_b' AND scope_id='msg-2'
                   AND source_session_id IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(null_identity, (None, 1), "the NULL row stays unqualified");

        // A later qualified alpha position 4 is still accepted against its
        // OWN mark (the NULL-lineage frame never gated it).
        assert!(
            insert(
                &conn,
                &qualified_record(3, "ins_t1p_b", "msg-2", "sess-alpha", 4, 110, 31)
            ),
            "the NULL lineage's position 1 does not block a later alpha frame"
        );
        let alpha_revision: i64 = conn
            .query_row(
                "SELECT metric_revision FROM usage_events
                 WHERE instance_id='ins_t1p_b' AND scope_id='msg-2'
                   AND source_session_id = 'sess-alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alpha_revision, 4);
    }

    #[test]
    fn a_high_unqualified_mark_does_not_block_a_fresh_qualified_lineage() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            insert(
                &conn,
                &scoped_record(
                    1,
                    "ins_t1p_c",
                    "turn",
                    Some("msg-3"),
                    9,
                    100,
                    20,
                    0,
                    0,
                    Some("2026-10-10T00:00:00.000Z")
                )
            ),
            "an unqualified frame sits at a high position"
        );
        assert!(
            insert(
                &conn,
                &qualified_record(2, "ins_t1p_c", "msg-3", "sess-alpha", 1, 60, 6)
            ),
            "the qualified lineage starts fresh at position 1 and is not frozen"
        );
        assert!(
            insert(
                &conn,
                &qualified_record(3, "ins_t1p_c", "msg-3", "sess-alpha", 2, 70, 7)
            ),
            "the qualified lineage advances from its own position 1"
        );
        // … while the NULL lineage still gates on its own high mark.
        assert!(
            !insert(
                &conn,
                &scoped_record(
                    4,
                    "ins_t1p_c",
                    "turn",
                    Some("msg-3"),
                    8,
                    99,
                    19,
                    0,
                    0,
                    Some("2026-10-10T00:00:00.000Z")
                )
            ),
            "the unqualified lineage keeps the numeric gate at 9"
        );
    }

    #[test]
    fn real_fixture_double_replay_is_durable() {
        use remuda_driver::TranscriptMapper;
        use remuda_protocol::{
            DriverKind, HostId, Id as PId, InstanceId, ObservationPayload, RunId,
        };
        use std::path::Path;

        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../remuda-journal/tests/fixtures/effort-21289");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&fixture_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        paths.sort();

        let instance = "ins_double_replay";
        let open_mapper = || {
            TranscriptMapper::new(
                DriverKind::ShellPty,
                InstanceId::new(),
                RunId::new(),
                PId::new("obj").unwrap(),
                HostId::new(),
                "double-replay-session".into(),
                "2.1.289".into(),
            )
        };
        let one_pass = |mapper: &mut TranscriptMapper,
                        conn: &Connection,
                        seq_start: i64|
         -> (i64, usize, PerScopeCounters) {
            let mut emitted = 0usize;
            let mut seq = seq_start;
            for path in &paths {
                let body = std::fs::read_to_string(path).unwrap();
                for line in body.lines().filter(|line| !line.trim().is_empty()) {
                    for obs in mapper.map_line(line).expect("fixture maps") {
                        if !matches!(obs.body, ObservationPayload::Usage(_)) {
                            continue;
                        }
                        let record = JournalRecord {
                            instance_id: instance.into(),
                            seq,
                            event_id: format!("evt_dr_{seq}"),
                            event: serde_json::to_value(&obs).unwrap(),
                            // Deliberately a fresh ingest time: the payload
                            // carries nativeAt, which must win.
                            observed_at: "2030-01-01T00:00:00.000Z".into(),
                        };
                        let row = project_usage_event(&record, None, None).unwrap();
                        insert_usage_event(conn, &row).unwrap();
                        emitted += 1;
                        seq += 1;
                    }
                }
                for obs in mapper.flush().expect("flush") {
                    if !matches!(obs.body, ObservationPayload::Usage(_)) {
                        continue;
                    }
                    let record = JournalRecord {
                        instance_id: instance.into(),
                        seq,
                        event_id: format!("evt_dr_{seq}"),
                        event: serde_json::to_value(&obs).unwrap(),
                        observed_at: "2030-01-01T00:00:00.000Z".into(),
                    };
                    let row = project_usage_event(&record, None, None).unwrap();
                    insert_usage_event(conn, &row).unwrap();
                    emitted += 1;
                    seq += 1;
                }
            }
            // Per-scope-id signature over ALL FOUR counters — a rewrite of
            // cache_read/cache_write/output (c-ctxusage r5 item 4) can no
            // longer slip through an input-only hash.
            let mut stmt = conn
                .prepare(
                    "SELECT scope_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
                     FROM usage_events
                     WHERE instance_id=?1 AND scope='turn'
                     ORDER BY scope_id",
                )
                .unwrap();
            let per_scope: BTreeMap<String, (i64, i64, i64, i64)> = stmt
                .query_map(params![instance], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .map(|row| {
                    let (scope, input, output, cache_read, cache_write) = row.unwrap();
                    (scope, (input, output, cache_read, cache_write))
                })
                .collect();
            assert_eq!(per_scope.len(), 24, "exactly 24 durable turn rows");
            (seq, emitted, per_scope)
        };

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let mut mapper = open_mapper();
        let (next_seq, emitted1, signature1) = one_pass(&mut mapper, &conn, 1);
        assert_eq!(
            signature1.len(),
            24,
            "pass 1 durably persists 24 turn rows (emitted {emitted1} usage obs)"
        );
        // The durable counters equal the independently parsed LAST usage per
        // message id from the raw fixture (c-ctxusage r5 item 4): all four
        // buckets, every one of the 24 ids.
        let expected = expected_usage_counters();
        assert_eq!(
            signature1, expected,
            "pass 1 counters match the raw fixture per scope_id"
        );

        // Second fresh mapper pass: byte-0 re-hydration with its own seqs.
        let mut mapper = open_mapper();
        let (_seq, emitted2, signature2) = one_pass(&mut mapper, &conn, next_seq);
        assert_eq!(
            signature2, signature1,
            "pass 2 leaves the SAME 24 rows and four counters (emitted {emitted2} obs)"
        );

        // Every durable row is native-timestamped, never the 2030 ingest value.
        let bad_times: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                     WHERE instance_id=?1 AND observed_at LIKE '2030-%'",
                params![instance],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bad_times, 0,
            "nativeAt wins over the ingest fallback on replay"
        );
    }

    type PerScopeCounters = BTreeMap<String, (i64, i64, i64, i64)>;

    fn expected_usage_counters() -> BTreeMap<String, (i64, i64, i64, i64)> {
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../remuda-journal/tests/fixtures/effort-21289");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&fixture_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        paths.sort();
        let mut expected: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
        for path in paths {
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let Ok(record) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if record.get("type").and_then(Value::as_str) != Some("assistant")
                    || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                let Some((id, usage)) = record
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .zip(record.pointer("/message/usage"))
                else {
                    continue;
                };
                let get = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
                let write = match (
                    usage
                        .pointer("/cache_creation/ephemeral_5m_input_tokens")
                        .and_then(Value::as_i64),
                    usage
                        .pointer("/cache_creation/ephemeral_1h_input_tokens")
                        .and_then(Value::as_i64),
                ) {
                    (Some(a), Some(b)) => a + b,
                    _ => get("cache_creation_input_tokens"),
                };
                expected.insert(
                    id.to_owned(),
                    (
                        get("input_tokens"),
                        get("output_tokens"),
                        get("cache_read_input_tokens"),
                        write,
                    ),
                );
            }
        }
        expected
    }

    // --- c-ctxusage r5 item 3 ----------------------------------------------

    fn create_journal_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE journal (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_id TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
            );",
        )
        .unwrap();
    }

    fn insert_journal_raw(conn: &Connection, record: &JournalRecord) {
        conn.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.instance_id,
                record.seq,
                record.event_id,
                serde_json::to_string(&record.event).unwrap(),
                record.observed_at
            ],
        )
        .unwrap();
    }

    #[test]
    fn two_codex_sessions_stocks_are_summed_with_no_boundary_increment() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let s = |seq: i64,
                 scope: &'static str,
                 input: i64,
                 output: i64,
                 at: &'static str|
         -> JournalRecord {
            scoped_record(
                seq,
                "ins_3a",
                "session",
                Some(scope),
                1,
                input,
                output,
                0,
                0,
                Some(at),
            )
        };
        // s1 grows 100 -> 300; s2 grows 50 -> 150 (interleaved native times).
        assert!(insert(
            &conn,
            &s(1, "s1", 100, 10, "2020-01-01T00:00:00.000Z")
        ));
        assert!(insert(
            &conn,
            &s(2, "s2", 50, 5, "2020-01-01T00:00:10.000Z")
        ));
        assert!(insert(
            &conn,
            &s(3, "s1", 300, 30, "2020-01-01T00:00:20.000Z")
        ));
        assert!(insert(
            &conn,
            &s(4, "s2", 150, 15, "2020-01-01T00:00:30.000Z")
        ));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_3a",
                kind: "codex",
                spec_model: None,
                effective_model: None,
                profile_id: Some("codex"),
            },
        )
        .unwrap()
        .unwrap();
        // Current stocks summed: 300 (s1) + 150 (s2), not "newest row = 150".
        assert_eq!(rollup.session_input_tokens, Some(450));
        assert_eq!(rollup.session_output_tokens, Some(45));
        // Historical points: no current-throughput claim.
        assert_eq!(rollup.tpm_in_60s, None);
    }

    #[test]
    fn legacy_null_scope_stock_never_floors_an_unrelated_scoped_replay() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(1, "ins_3b", 1000, 100, "2020-01-01T00:00:00.000Z")
            )
            .unwrap()
        );
        // A scoped replay below the legacy number lands: identity unprovable.
        let replay_smaller =
            scoped_record(10, "ins_3b", "session", Some("s1"), 1, 100, 10, 0, 0, None);
        assert!(
            insert(&conn, &replay_smaller),
            "an unprovably-related scoped stock must not be frozen by the legacy row"
        );
        let current = scoped_record(
            11,
            "ins_3b",
            "session",
            Some("s1"),
            1,
            1200,
            120,
            0,
            0,
            None,
        );
        assert!(
            insert(&conn, &current),
            "growth above the legacy floor appends"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_3b",
                kind: "codex",
                spec_model: None,
                effective_model: None,
                profile_id: Some("codex"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(1200),
            "current stock follows the newest native-time scoped point"
        );
    }

    #[test]
    fn legacy_row_without_a_provable_journal_scope_stays_null() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(1, "ins_5c", 1000, 100, "2020-01-01T00:00:00.000Z")
            )
            .unwrap()
        );
        // Journal event present but with NO scopeId (a genuinely pre-scopeId
        // payload): identity is unprovable.
        let unproven = scoped_record(1, "ins_5c", "session", None, 1, 1000, 100, 0, 0, None);
        insert_journal_raw(&conn, &unproven);
        migrate(&conn).unwrap();
        assert_eq!(scope_id_at(&conn, 1), None);
    }

    #[test]
    fn old_schema_later_smaller_replay_is_normalised_to_the_high_water_stock() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                scope_id TEXT,
                mode TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );
             CREATE UNIQUE INDEX usage_events_scope_dedupe
                 ON usage_events(instance_id, scope, scope_id)
                 WHERE scope_id IS NOT NULL;",
        )
        .unwrap();
        // The high-water stock lands first; a smaller HISTORICAL replay lands
        // afterwards at a higher seq/rowid (the old code inserted both).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, mode, total_tokens,
                 input_tokens, output_tokens, accounting, observed_at)
             VALUES ('ins_r11', 1, 'session', NULL, 'snapshot',
                     220, 200, 20, 'estimated', '2020-01-01T00:00:20.000Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, mode, total_tokens,
                 input_tokens, output_tokens, accounting, observed_at)
             VALUES ('ins_r11', 2, 'session', NULL, 'snapshot',
                     110, 100, 10, 'estimated', '2020-01-01T00:00:10.000Z')",
            [],
        )
        .unwrap();
        create_journal_table(&conn);
        insert_journal_raw(
            &conn,
            &scoped_record(1, "ins_r11", "session", Some("s1"), 1, 200, 20, 0, 0, None),
        );
        insert_journal_raw(
            &conn,
            &scoped_record(2, "ins_r11", "session", Some("s1"), 1, 100, 10, 0, 0, None),
        );

        migrate(&conn).unwrap();

        // Both rows backfill to s1, then the backwards 100/10 replay is
        // rejected by the replayed acceptance rule: only 200/20 survives.
        let remaining: Vec<(i64, i64)> = conn
            .prepare(
                "SELECT input_tokens, output_tokens FROM usage_events
                 WHERE instance_id = 'ins_r11' AND scope = 'session'
                 ORDER BY rowid ASC",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            remaining,
            vec![(200, 20)],
            "the stale smaller replay is normalised away; the high-water stays"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r11",
                kind: "codex",
                spec_model: Some("codex"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(200),
            "current stock is the proven high-water point, not the last rowid"
        );
        assert_eq!(rollup.session_output_tokens, Some(20));

        // Floors and current stock now agree: 150/15 is a backwards point in
        // the SAME proven session and stays frozen; genuine growth is accepted.
        assert!(
            !insert(
                &conn,
                &scoped_record(3, "ins_r11", "session", Some("s1"), 1, 150, 15, 0, 0, None)
            ),
            "a backwards live point is still frozen by the retained high-water"
        );
        assert!(insert(
            &conn,
            &scoped_record(4, "ins_r11", "session", Some("s1"), 1, 250, 25, 0, 0, None)
        ));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r11",
                kind: "codex",
                spec_model: Some("codex"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(250));
        assert_eq!(rollup.session_output_tokens, Some(25));

        // Idempotent: a second open neither revives nor deletes anything.
        migrate(&conn).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = 'ins_r11' AND scope = 'session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "200/20 high-water plus the accepted 250/25 growth");
    }

    #[test]
    fn first_small_scoped_session_is_counted_against_legacy_stock() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // One legacy cumulative row for a former session: 1000/100.
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(1, "ins_5", 1000, 100, "2020-01-01T00:00:00.000Z")
            )
            .unwrap()
        );
        // First scoped session at 500: counted (identity cannot be proven).
        assert!(
            insert(
                &conn,
                &scoped_record(10, "ins_5", "session", Some("s1"), 1, 500, 50, 0, 0, None)
            ),
            "the first scoped session must not be floored by the legacy row"
        );
        assert!(insert(
            &conn,
            &scoped_record(11, "ins_5", "session", Some("s1"), 1, 1200, 120, 0, 0, None)
        ));

        // A different later session with a much smaller stock: counted too.
        assert!(
            insert(
                &conn,
                &scoped_record(12, "ins_5", "session", Some("s2"), 1, 100, 10, 0, 0, None)
            ),
            "a later scoped session must not be floored by another session's legacy total"
        );
        // Its own per-scope-id floor still works: a shrink within s2 freezes.
        assert!(
            !insert(
                &conn,
                &scoped_record(13, "ins_5", "session", Some("s2"), 1, 80, 8, 0, 0, None)
            ),
            "the own-scope-id floor still freezes a backwards s2 point"
        );
        // s2's current stock is exactly its own point.
        let points = conn
            .query_row(
                "SELECT input_tokens FROM usage_events
                 WHERE instance_id = 'ins_5' AND scope = 'session' AND scope_id = 's2'
                 ORDER BY observed_at DESC, seq DESC LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(points, 100);
    }

    #[test]
    fn ingest_stamped_codex_replay_stays_out_of_rate_windows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Two turn rows stamped "now" as ingest fallback (project sets source
        // to 'ingest' when no nativeAt is present).
        let now = {
            let t = time::OffsetDateTime::now_utc();
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                t.millisecond()
            )
        };
        let ingest_turn = |seq: i64, input: i64, output: i64| JournalRecord {
            instance_id: "ins_r57".into(),
            seq,
            event_id: format!("evt7_{seq}"),
            event: json!({
                "kind": "usage",
                "payload": {
                    "scope": "turn",
                    "mode": "snapshot",
                    "scopeId": format!("ing-{seq}"),
                    "metricRevision": "1",
                    "inputTokens": { "state": "known", "value": input.to_string() },
                    "outputTokens": { "state": "known", "value": output.to_string() },
                    "cacheReadTokens": { "state": "known", "value": "0" },
                    "cacheWriteTokens": { "state": "known", "value": "0" },
                    "cost": { "state": "unknown", "reason": "u", "evidenceEventIds": [] },
                    "accounting": "estimated"
                }
            }),
            observed_at: now.clone(),
        };
        assert!(
            insert_usage_event(
                &conn,
                &project_usage_event(&ingest_turn(1, 100, 10), None, None).unwrap()
            )
            .unwrap()
        );
        assert!(
            insert_usage_event(
                &conn,
                &project_usage_event(&ingest_turn(2, 200, 20), None, None).unwrap()
            )
            .unwrap()
        );

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r57",
                kind: "codex",
                spec_model: None,
                effective_model: None,
                profile_id: Some("codex"),
            },
        )
        .unwrap()
        .unwrap();
        // Totals still count; current windows and lastTurnAt do not.
        assert_eq!(rollup.session_input_tokens, Some(300));
        assert_eq!(rollup.session_output_tokens, Some(30));
        assert_eq!(
            rollup.tpm_in_60s, None,
            "ingest replay is not current throughput"
        );
        assert_eq!(rollup.tpm_in_5m, None);
        assert_eq!(
            rollup.last_turn_at, None,
            "no trustworthy lastTurnAt without native time"
        );

        // A later real turn with a native timestamp is window-eligible.
        let native = scoped_record(
            3,
            "ins_r57",
            "turn",
            Some("real-1"),
            1,
            40,
            4,
            0,
            0,
            Some(&now), // scoped_record marks nativeAt -> source native
        );
        assert!(insert(&conn, &native));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r57",
                kind: "codex",
                spec_model: None,
                effective_model: None,
                profile_id: Some("codex"),
            },
        )
        .unwrap()
        .unwrap();
        // Only the native 40 is in-window; the ingest rows still count totals.
        assert_eq!(
            rollup.tpm_in_60s,
            Some(40),
            "only the native turn enters the window"
        );
        assert_eq!(rollup.tpm_out_60s, Some(4));
        assert_eq!(rollup.session_input_tokens, Some(340));
        assert_eq!(rollup.last_turn_at.as_deref(), Some(now.as_str()));
    }

    #[test]
    fn live_print_result_usage_enters_windows_historical_replay_stays_out() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Bulk replay first: two old turns with their real (hours-old) native
        // times, ingested now — same counters, old stamps.
        let old1 = ago(7_200);
        let old2 = ago(7_000);
        let replay1 = scoped_record(
            1,
            "ins_live",
            "turn",
            Some("replay-1"),
            1,
            500,
            50,
            0,
            0,
            Some(&old1),
        );
        let replay2 = scoped_record(
            2,
            "ins_live",
            "turn",
            Some("replay-2"),
            1,
            700,
            70,
            0,
            0,
            Some(&old2),
        );
        assert!(insert(&conn, &replay1));
        assert!(insert(&conn, &replay2));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_live",
                kind: "claude",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 2, "replayed turns count as turns");
        assert_eq!(
            rollup.session_input_tokens,
            Some(1_200),
            "totals fold history"
        );
        assert_eq!(rollup.tpm_in_60s, None, "history is not current throughput");
        assert_eq!(rollup.tpm_in_5m, None);
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some(old2.as_str()),
            "the newest NATIVE time still wins, even if historical"
        );

        // Now the live print result: the driver stamps nativeAt = receipt.
        let receipt = ago(2);
        let live = scoped_record(
            3,
            "ins_live",
            "turn",
            Some("live-result"),
            1,
            250,
            25,
            0,
            0,
            Some(&receipt),
        );
        // Sanity: projection really treated the stamped time as native.
        assert_eq!(
            project_usage_event(&live, None, None)
                .unwrap()
                .observed_at_source,
            "native",
            "a stamped live result is a native-time row"
        );
        assert!(insert(&conn, &live));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_live",
                kind: "claude",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 3);
        assert_eq!(rollup.session_input_tokens, Some(1_450));
        assert_eq!(
            rollup.tpm_in_60s,
            Some(250),
            "the live receipt-time turn enters the 60 s window; history does not"
        );
        assert_eq!(rollup.tpm_out_60s, Some(25));
        assert_eq!(rollup.tpm_in_5m, Some(50), "250 over the 5-minute average");
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some(receipt.as_str()),
            "lastTurnAt follows the live receipt time"
        );
    }

    #[test]
    fn ingest_session_replay_does_not_author_last_turn_at_or_rates() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let now = {
            let t = time::OffsetDateTime::now_utc();
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                t.millisecond()
            )
        };

        // Session-only harness (e.g. Grok): one live NATIVE stock, then a
        // restart replay of historical Session rows WITHOUT nativeAt (ingest
        // fallback at 2026-09-15 — "now" per the test helper's observed_at).
        let native_old = "2026-09-14T12:00:00.000Z";
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_9",
                "session",
                Some("g1"),
                1,
                100,
                10,
                0,
                0,
                Some(native_old)
            )
        ));
        // Two timestamp-free replay growth points (would be within any window
        // if their ingest time counted) — including a fresh larger stock.
        assert!(insert(
            &conn,
            &scoped_record(10, "ins_9", "session", Some("g1"), 2, 120, 12, 0, 0, None)
        ));
        assert!(insert(
            &conn,
            &scoped_record(11, "ins_9", "session", Some("g1"), 3, 140, 14, 0, 0, None)
        ));

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_9",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        // lastTurnAt tracks the native time, never the ingest fallback.
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some(native_old),
            "ingest-fallback session rows must not restore the ingest time"
        );
        // Rates key off native-timestamped points only; the replay increments
        // (100 -> 120 -> 140 at ingest "now") must not read as throughput.
        // The single native point has no predecessor, so the rate is unknown.
        assert_eq!(rollup.tpm_in_60s, None, "ingest replay is not throughput");
        assert_eq!(rollup.tpm_out_60s, None);

        // A fresh LIVE session point with a native time "now" takes over both
        // lastTurnAt and the windows: its increment vs the previous NATIVE
        // stock is the rate.
        assert!(insert(
            &conn,
            &scoped_record(
                12,
                "ins_9",
                "session",
                Some("g1"),
                4,
                150,
                15,
                0,
                0,
                Some(&now)
            )
        ));
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_9",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.last_turn_at.as_deref(),
            Some(now.as_str()),
            "a new native point updates lastTurnAt"
        );
        assert_eq!(
            rollup.tpm_in_60s,
            Some(10),
            "150 minus the IMMEDIATE predecessor 140 (an ingest replay that \
             still advances the baseline); not 150 minus the stale native 100"
        );
        assert_eq!(rollup.tpm_out_60s, Some(1));
    }

    #[test]
    fn ingest_replay_points_advance_the_session_diff_baseline() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let now = {
            let t = time::OffsetDateTime::now_utc();
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                t.millisecond()
            )
        };
        // Native live point, then two timestamp-free replay growth points,
        // then a fresh native live point — exactly the r8 shape.
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_cx",
                "session",
                Some("c1"),
                1,
                100,
                10,
                0,
                0,
                Some("2026-09-14T12:00:00.000Z")
            )
        ));
        assert!(insert(
            &conn,
            &scoped_record(2, "ins_cx", "session", Some("c1"), 2, 120, 12, 0, 0, None)
        ));
        assert!(insert(
            &conn,
            &scoped_record(3, "ins_cx", "session", Some("c1"), 3, 140, 14, 0, 0, None)
        ));
        assert!(insert(
            &conn,
            &scoped_record(
                4,
                "ins_cx",
                "session",
                Some("c1"),
                4,
                150,
                15,
                0,
                0,
                Some(&now)
            )
        ));
        // The Turn snapshot the real Codex adapter emits beside the Session
        // snapshot at the same turn end (native time, per-call flow),
        // qualified to native session c1.
        assert!(insert(
            &conn,
            &with_source_session(
                scoped_record(
                    5,
                    "ins_cx",
                    "turn",
                    Some("msg_cx_t1"),
                    1,
                    7,
                    2,
                    0,
                    0,
                    Some(&now)
                ),
                "c1"
            )
        ));

        let increments = session_stock_increments(&conn, "ins_cx").unwrap();
        // Only the final native point emits, diffed against its IMMEDIATE
        // predecessor (the ingest 140), not the stale native 100: 10, not 50.
        assert_eq!(increments.len(), 1, "one native-ended interval");
        assert_eq!(increments[0].input, Some(10));
        assert_eq!(increments[0].output, Some(1));
        assert_eq!(increments[0].end_at, now);

        // With a Turn row present the rollup rates come from the per-call
        // turn flow; the session stock stays the current 150/15 regardless.
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_cx",
                kind: "codex",
                spec_model: Some("codex"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(150));
        assert_eq!(rollup.session_output_tokens, Some(15));
        assert_eq!(rollup.tpm_in_60s, Some(7));
        assert_eq!(rollup.tpm_out_60s, Some(2));
    }

    fn stamp_rel(delta: i64) -> String {
        let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(delta);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.millisecond()
        )
    }

    #[test]
    fn late_accepted_native_point_chains_in_acceptance_order() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Accepted order: ingest(10, now+10s), ingest(50, now+11s), then the
        // buffered native point (150, now-2s). Mixed clocks.
        let mut first = scoped_record(1, "ins_o3", "session", Some("s1"), 1, 10, 1, 0, 0, None);
        first.observed_at = stamp_rel(10);
        assert!(insert(&conn, &first));
        let mut second = scoped_record(2, "ins_o3", "session", Some("s1"), 2, 50, 5, 0, 0, None);
        second.observed_at = stamp_rel(11);
        assert!(insert(&conn, &second));
        let native_at = stamp_rel(-2);
        assert!(insert(
            &conn,
            &scoped_record(
                3,
                "ins_o3",
                "session",
                Some("s1"),
                3,
                150,
                15,
                0,
                0,
                Some(&native_at)
            )
        ));

        let increments = session_stock_increments(&conn, "ins_o3").unwrap();
        // One native-ended interval: 150 - its ACCEPTED predecessor 50 = 100.
        assert_eq!(increments.len(), 1);
        assert_eq!(increments[0].input, Some(100), "150-50, not 150-10");
        assert_eq!(increments[0].output, Some(10));
        assert_eq!(increments[0].end_at, native_at);

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_o3",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(150),
            "current stock is the last ACCEPTED point, not the newest-clock ingest 50"
        );
        assert_eq!(rollup.session_output_tokens, Some(15));
        // The native end time is 2 s ago: inside the 60 s rate window.
        assert_eq!(rollup.tpm_in_60s, Some(100));
        assert_eq!(rollup.tpm_out_60s, Some(10));
        assert_eq!(rollup.last_turn_at.as_deref(), Some(native_at.as_str()));
    }

    #[test]
    fn an_unknown_predecessor_counter_stays_unknown_for_that_bucket() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let t1 = stamp_rel(-3);
        let t2 = stamp_rel(-1);
        // First native point reports output only: input is genuinely unknown.
        let mut first = scoped_record(
            1,
            "ins_o4",
            "session",
            Some("s1"),
            1,
            0,
            10,
            0,
            0,
            Some(&t1),
        );
        first.event["payload"]["inputTokens"] =
            json!({ "state": "unknown", "reason": "unreported" });
        assert!(insert(&conn, &first));
        assert!(insert(
            &conn,
            &scoped_record(
                2,
                "ins_o4",
                "session",
                Some("s1"),
                2,
                150,
                15,
                0,
                0,
                Some(&t2)
            )
        ));

        let increments = session_stock_increments(&conn, "ins_o4").unwrap();
        assert_eq!(increments.len(), 1);
        assert_eq!(
            increments[0].input, None,
            "unknown→150 is unknown, not a 150 increment"
        );
        assert_eq!(increments[0].output, Some(5));

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_o4",
                kind: "grok",
                spec_model: None,
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        // The cumulative stock itself is still reported at 150.
        assert_eq!(rollup.session_input_tokens, Some(150));
        assert_eq!(rollup.session_output_tokens, Some(15));
        assert_eq!(rollup.tpm_in_60s, None);
        assert_eq!(rollup.tpm_out_60s, Some(5));
    }

    #[allow(clippy::too_many_arguments)]
    fn aggregate_turn_row(
        seq: i64,
        scope_id: &str,
        total: Option<i64>,
        input: Option<i64>,
        output: Option<i64>,
        cache_read: Option<i64>,
        cache_write: Option<i64>,
        cost: Option<&str>,
        profile: Option<&str>,
        model: Option<&str>,
    ) -> UsageEventRow {
        UsageEventRow {
            instance_id: "ins_u1".into(),
            seq,
            profile_id: profile.map(str::to_string),
            model: model.map(str::to_string),
            scope: "turn".into(),
            scope_id: Some(scope_id.into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: None,
            total_tokens: total,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cost_usd: cost.map(str::to_string),
            accounting: "estimated".into(),
            observed_at: "2026-10-10T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        }
    }

    #[test]
    fn aggregate_input_is_unknown_when_any_turn_omits_it() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Turn 1 fully reported; turn 2 reports output only.
        insert_usage_event(
            &conn,
            &aggregate_turn_row(
                1,
                "t1",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                None,
                None,
                None,
            ),
        )
        .unwrap();
        insert_usage_event(
            &conn,
            &aggregate_turn_row(2, "t2", None, None, Some(20), None, None, None, None, None),
        )
        .unwrap();
        let agg = aggregate_instance(&conn, "ins_u1").unwrap();
        assert_eq!(agg.events, 2);
        assert_eq!(
            agg.input_tokens, None,
            "one turn omitted input: Unknown, not 100"
        );
        assert_eq!(agg.output_tokens, Some(30), "output was reported by both");
        assert_eq!(agg.total_tokens, None, "the total needs every bucket");
    }

    #[test]
    fn an_unpriced_turn_makes_the_bill_and_budget_status_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        insert_usage_event(
            &conn,
            &aggregate_turn_row(
                1,
                "p1",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                Some("0.50"),
                None,
                None,
            ),
        )
        .unwrap();
        insert_usage_event(
            &conn,
            &aggregate_turn_row(
                2,
                "p2",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                None,
                None,
                None,
            ),
        )
        .unwrap();
        let agg = aggregate_instance(&conn, "ins_u1").unwrap();
        assert_eq!(
            agg.cost_usd, None,
            "one unpriced turn makes the bill Unknown, not 0.50"
        );
        for cap in [None, Some(1_000.0), Some(0.5), Some(0.01)] {
            assert_eq!(
                agg.budget_status(cap),
                BudgetStatus::Unknown,
                "an unknown bill never certifies Ok under any cap"
            );
        }

        // Two fully-priced turns sum to a known bill and an Ok status under a
        // generous cap (Unknown is not contagious without an unpriced row).
        let conn2 = Connection::open_in_memory().unwrap();
        migrate(&conn2).unwrap();
        insert_usage_event(
            &conn2,
            &aggregate_turn_row(
                1,
                "p1",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                Some("0.50"),
                None,
                None,
            ),
        )
        .unwrap();
        insert_usage_event(
            &conn2,
            &aggregate_turn_row(
                2,
                "p2",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                Some("0.50"),
                None,
                None,
            ),
        )
        .unwrap();
        let priced = aggregate_instance(&conn2, "ins_u1").unwrap();
        assert_eq!(priced.cost_usd, Some(1.0));
        assert_eq!(priced.budget_status(Some(10.0)), BudgetStatus::Ok);
        assert_eq!(priced.budget_status(Some(1.0)), BudgetStatus::Warn);
        assert_eq!(priced.budget_status(Some(0.5)), BudgetStatus::Stop);
        assert_eq!(priced.budget_status(None), BudgetStatus::Ok);
    }

    #[test]
    fn aggregate_supply_cost_is_unknown_with_any_unpriced_row() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile = "pvp_supply_u1";
        insert_usage_event(
            &conn,
            &aggregate_turn_row(
                1,
                "t1",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                Some("1.00"),
                Some(profile),
                Some("m"),
            ),
        )
        .unwrap();
        insert_usage_event(
            &conn,
            &aggregate_turn_row(
                2,
                "t2",
                Some(110),
                Some(100),
                Some(10),
                Some(0),
                Some(0),
                None,
                Some(profile),
                Some("m"),
            ),
        )
        .unwrap();
        let agg = aggregate_supply(&conn, profile, "m").unwrap();
        assert_eq!(agg.cost_usd, None);
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Unknown);
    }

    #[test]
    fn a_superseded_model_contributes_zero_events_to_its_supply() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile_a = "pvp_sup_a";
        let profile_b = "pvp_sup_b";
        let model_a = "model-a";
        let model_b = "model-b";

        // First accepted frame for msg-1 under A (lineage alpha).
        let mut under_a = aggregate_turn_row(
            10,
            "msg-1",
            Some(100),
            Some(100),
            Some(0),
            Some(0),
            Some(0),
            Some("1.00"),
            Some(profile_a),
            Some(model_a),
        );
        under_a.instance_id = "ins_sup".into();
        under_a.source_session_id = Some("alpha".into());
        assert!(insert_usage_event(&conn, &under_a).unwrap());

        // A strictly newer accepted frame for the SAME message id under B
        // (lineage beta) — the global winner for the scope id.
        let mut under_b = aggregate_turn_row(
            20,
            "msg-1",
            Some(200),
            Some(200),
            Some(0),
            Some(0),
            Some(0),
            Some("2.00"),
            Some(profile_b),
            Some(model_b),
        );
        under_b.instance_id = "ins_sup".into();
        under_b.source_session_id = Some("beta".into());
        assert!(insert_usage_event(&conn, &under_b).unwrap());

        let supply_a = aggregate_supply(&conn, profile_a, model_a).unwrap();
        assert_eq!(
            supply_a.events, 0,
            "the superseded A row must not count in A's provider supply"
        );
        assert_eq!(supply_a.total_tokens, None);
        assert_eq!(supply_a.cost_usd, None);

        let supply_b = aggregate_supply(&conn, profile_b, model_b).unwrap();
        assert_eq!(supply_b.events, 1, "B owns the winning row");
        assert_eq!(supply_b.total_tokens, Some(200));
        assert_eq!(supply_b.cost_usd, Some(2.0));

        // The instance-level fold still counts the logical turn exactly once.
        let instance = aggregate_instance(&conn, "ins_sup").unwrap();
        assert_eq!(instance.events, 1, "one logical turn across both lineages");
        assert_eq!(instance.total_tokens, Some(200));
        assert_eq!(instance.cost_usd, Some(2.0));
    }

    #[test]
    fn a_legacy_session_in_another_instance_does_not_suppress_a_legacy_turn() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile = "pvp_legacy_scope";
        let model = "m-legacy";

        // Instance A: one unscoped legacy TURN, $1.
        let legacy_turn = UsageEventRow {
            instance_id: "ins_legacy_a".into(),
            seq: 1,
            profile_id: Some(profile.into()),
            model: Some(model.into()),
            scope: "turn".into(),
            scope_id: None,
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: None,
            total_tokens: Some(110),
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("1.00".into()),
            accounting: "estimated".into(),
            observed_at: "2026-10-13T00:00:00.000Z".into(),
            observed_at_source: "ingest".into(),
        };
        assert!(insert_usage_event(&conn, &legacy_turn).unwrap());

        // Instance B: one unscoped legacy cumulative SESSION row, $2.
        let legacy_session = UsageEventRow {
            turn_id: None,
            native_context_window: None,
            instance_id: "ins_legacy_b".into(),
            scope: "session".into(),
            seq: 1,
            ..legacy_turn.clone()
        };
        let legacy_session = UsageEventRow {
            turn_id: None,
            native_context_window: None,
            total_tokens: Some(220),
            input_tokens: Some(200),
            output_tokens: Some(20),
            cost_usd: Some("2.00".into()),
            ..legacy_session
        };
        assert!(insert_usage_event(&conn, &legacy_session).unwrap());

        let supply = aggregate_supply(&conn, profile, model).unwrap();
        assert_eq!(supply.events, 2, "$1 turn and $2 session are distinct rows");
        assert_eq!(supply.total_tokens, Some(330));
        assert_eq!(supply.input_tokens, Some(300));
        assert_eq!(supply.output_tokens, Some(30));
        assert_eq!(supply.cost_usd, Some(3.0));

        // Each instance-level fold independently sees only its own row.
        assert_eq!(aggregate_instance(&conn, "ins_legacy_a").unwrap().events, 1);
        assert_eq!(aggregate_instance(&conn, "ins_legacy_b").unwrap().events, 1);
    }

    #[test]
    fn a_scoped_session_in_another_instance_does_not_suppress_a_scoped_turn() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile = "pvp_scoped_scope";
        let model = "m-scoped";

        // Instance A: a scoped turn attributed to native session "sess-x".
        let turn = UsageEventRow {
            instance_id: "ins_scoped_a".into(),
            seq: 1,
            profile_id: Some(profile.into()),
            model: Some(model.into()),
            scope: "turn".into(),
            scope_id: Some("msg-1".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: Some("sess-x".into()),
            total_tokens: Some(101),
            input_tokens: Some(100),
            output_tokens: Some(1),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("1.00".into()),
            accounting: "estimated".into(),
            observed_at: "2026-10-13T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        };
        assert!(insert_usage_event(&conn, &turn).unwrap());

        // Instance B: a winning session stock for the SAME native session id,
        // unrelated to A's turn.
        let session = UsageEventRow {
            instance_id: "ins_scoped_b".into(),
            seq: 1,
            scope: "session".into(),
            scope_id: Some("sess-x".into()),
            turn_id: None,
            native_context_window: None,
            source_session_id: Some("sess-x".into()),
            ..turn.clone()
        };
        assert!(insert_usage_event(&conn, &session).unwrap());

        let supply = aggregate_supply(&conn, profile, model).unwrap();
        assert_eq!(
            supply.events, 2,
            "B's session stock never covers A's turn across instances"
        );

        // Same instance + same native session: coverage still applies (the
        // Codex turn+stock shape folds the turn away).
        let session_same = UsageEventRow {
            instance_id: "ins_scoped_a".into(),
            seq: 2,
            scope: "session".into(),
            scope_id: Some("sess-x".into()),
            turn_id: None,
            native_context_window: None,
            source_session_id: Some("sess-x".into()),
            ..turn
        };
        assert!(insert_usage_event(&conn, &session_same).unwrap());
        let supply = aggregate_supply(&conn, profile, model).unwrap();
        assert_eq!(
            supply.events, 2,
            "A's turn is covered by A's own stock; B's stock still counts"
        );
    }

    #[test]
    fn two_null_scope_stocks_fold_to_the_newest_in_every_fold() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile = "pvp_null2";
        let model = "m-null2";
        let row = |seq: i64, input: i64, output: i64| UsageEventRow {
            instance_id: "ins_null2".into(),
            seq,
            profile_id: Some(profile.into()),
            model: Some(model.into()),
            scope: "session".into(),
            scope_id: None,
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: None,
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: format!("2026-10-13T00:00:0{seq}.000Z"),
            observed_at_source: "ingest".into(),
        };
        assert!(insert_usage_event(&conn, &row(1, 100, 10)).unwrap());
        // Monotone growth: the second cumulative point is accepted.
        assert!(insert_usage_event(&conn, &row(2, 150, 15)).unwrap());

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_null2",
                kind: "codex",
                spec_model: Some(model),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(150), "newest stock");
        assert_eq!(rollup.session_output_tokens, Some(15));

        let agg = aggregate_instance(&conn, "ins_null2").unwrap();
        assert_eq!(agg.events, 1, "one cumulative chain = one event");
        assert_eq!(agg.total_tokens, Some(165));
        assert_eq!(agg.input_tokens, Some(150));
        assert_eq!(agg.output_tokens, Some(15));

        let supply = aggregate_supply(&conn, profile, model).unwrap();
        assert_eq!(supply.events, 1);
        assert_eq!(supply.total_tokens, Some(165));
        assert_eq!(supply.input_tokens, Some(150));
        assert_eq!(supply.output_tokens, Some(15));
    }
    fn create_r28_usage_table_with_turn_index(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE usage_events (
                instance_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                profile_id TEXT,
                model TEXT,
                scope TEXT NOT NULL,
                scope_id TEXT,
                mode TEXT NOT NULL,
                metric_revision INTEGER NOT NULL DEFAULT 1,
                source_session_id TEXT,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                observed_at_source TEXT NOT NULL DEFAULT 'ingest',
                PRIMARY KEY (instance_id, seq)
             );
             CREATE UNIQUE INDEX usage_events_turn_dedupe
                 ON usage_events(instance_id, scope, scope_id,
                                 COALESCE(source_session_id, ''))
                 WHERE scope = 'turn' AND scope_id IS NOT NULL;",
        )
        .unwrap();
    }

    #[test]
    fn same_session_stock_covers_turns_after_a_model_change_in_supply() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile = "pvp_codemodel";
        let model_a = "model-a";
        let model_b = "model-b";
        let instance = "ins_codemodel";
        let session = "sess-codex-same";

        // Turn 1 under model A: 100 tokens / $1, qualified to session S.
        let turn_a = UsageEventRow {
            instance_id: instance.into(),
            seq: 1,
            profile_id: Some(profile.into()),
            model: Some(model_a.into()),
            scope: "turn".into(),
            scope_id: Some("turn-1".into()),
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: Some(session.into()),
            total_tokens: Some(110),
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("1.00".into()),
            accounting: "estimated".into(),
            observed_at: "2026-10-14T00:00:00.000Z".into(),
            observed_at_source: "native".into(),
        };
        // Cumulative stock after turn 1, emitted under the then-current A.
        let stock_a = UsageEventRow {
            seq: 2,
            scope: "session".into(),
            scope_id: Some(session.into()),
            turn_id: None,
            native_context_window: None,
            ..turn_a.clone()
        };
        assert!(insert_usage_event(&conn, &turn_a).unwrap());
        assert!(insert_usage_event(&conn, &stock_a).unwrap());

        // Reconfigure to model B; turn 2: 150 / $2 cumulative. The new stock
        // is 250 / $3 (contains the model-A turn), projected under B.
        let turn_b = UsageEventRow {
            seq: 3,
            model: Some(model_b.into()),
            scope: "turn".into(),
            scope_id: Some("turn-2".into()),
            turn_id: None,
            native_context_window: None,
            metric_revision: 1,
            total_tokens: Some(170),
            input_tokens: Some(150),
            output_tokens: Some(20),
            cost_usd: Some("2.00".into()),
            observed_at: "2026-10-14T00:00:05.000Z".into(),
            ..turn_a.clone()
        };
        let stock_b = UsageEventRow {
            seq: 4,
            model: Some(model_b.into()),
            scope: "session".into(),
            scope_id: Some(session.into()),
            turn_id: None,
            native_context_window: None,
            total_tokens: Some(280),
            input_tokens: Some(250),
            output_tokens: Some(30),
            cost_usd: Some("3.00".into()),
            observed_at: "2026-10-14T00:00:05.000Z".into(),
            ..turn_b.clone()
        };
        assert!(insert_usage_event(&conn, &turn_b).unwrap());
        assert!(insert_usage_event(&conn, &stock_b).unwrap());

        // The instance fold: one winning stock = 250 / $3, both turns covered.
        let instance_agg = aggregate_instance(&conn, instance).unwrap();
        assert_eq!(instance_agg.events, 1, "only the winning stock contributes");
        assert_eq!(instance_agg.total_tokens, Some(280));
        assert_eq!(instance_agg.input_tokens, Some(250));
        assert_eq!(instance_agg.output_tokens, Some(30));
        assert_eq!(instance_agg.cost_usd, Some(3.0));

        // Supply(A): the model-A turn is GLOBALLY covered by the session's
        // winning (model-B) stock, and the older A stock is not a winner —
        // nothing for A.
        let supply_a = aggregate_supply(&conn, profile, model_a).unwrap();
        assert_eq!(
            supply_a.events, 0,
            "A's turn is inside B's cumulative stock"
        );
        assert_eq!(supply_a.input_tokens, None);
        assert_eq!(supply_a.cost_usd, None);

        // Supply(B): the winning stock for S, 250 / $3.
        let supply_b = aggregate_supply(&conn, profile, model_b).unwrap();
        assert_eq!(supply_b.events, 1);
        assert_eq!(supply_b.total_tokens, Some(280));
        assert_eq!(supply_b.input_tokens, Some(250));
        assert_eq!(supply_b.output_tokens, Some(30));
        assert_eq!(supply_b.cost_usd, Some(3.0));

        // The invariant: per-model rows partition the instance total exactly.
        assert_eq!(
            supply_a.events + supply_b.events,
            instance_agg.events,
            "per-model supply sums equal the instance fold"
        );
        assert_eq!(supply_b.input_tokens, instance_agg.input_tokens);
        assert_eq!(supply_b.cost_usd, instance_agg.cost_usd);
    }

    #[test]
    fn ambiguous_only_model_usage_is_flagged_not_dropped_in_supply() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile = "pvp_ambig_model";
        let model_a = "model-a";
        let model_b = "model-b";
        let instance = "ins_ambig_model";
        let session = "sess-b-scoped";

        // Retained model-A turn with NULL lineage (the unprovable shape).
        let ambiguous_turn = UsageEventRow {
            instance_id: instance.into(),
            seq: 1,
            profile_id: Some(profile.into()),
            model: Some(model_a.into()),
            scope: "turn".into(),
            scope_id: None,
            turn_id: None,
            native_context_window: None,
            mode: "snapshot".into(),
            metric_revision: 1,
            source_session_id: None,
            total_tokens: Some(110),
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: Some("1.00".into()),
            accounting: "estimated".into(),
            observed_at: "2026-10-14T00:00:00.000Z".into(),
            observed_at_source: "ingest".into(),
        };
        assert!(insert_usage_event(&conn, &ambiguous_turn).unwrap());

        // Scoped model-B cumulative stock (contains whatever B ran).
        let stock_b = UsageEventRow {
            seq: 2,
            model: Some(model_b.into()),
            scope: "session".into(),
            scope_id: Some(session.into()),
            turn_id: None,
            native_context_window: None,
            source_session_id: Some(session.into()),
            total_tokens: Some(280),
            input_tokens: Some(250),
            output_tokens: Some(30),
            cost_usd: Some("3.00".into()),
            ..ambiguous_turn.clone()
        };
        assert!(insert_usage_event(&conn, &stock_b).unwrap());

        // Instance totals: one proven stock; the ambiguous turn poisons the
        // numeric buckets (it reported values that cannot be certified absent
        // or covered).
        let instance_agg = aggregate_instance(&conn, instance).unwrap();
        assert_eq!(instance_agg.events, 1, "only the stock is a proven row");
        assert_eq!(instance_agg.ambiguous_events, 1);
        assert!(instance_agg.ambiguous);
        assert_eq!(
            instance_agg.input_tokens, None,
            "the ambiguous turn keeps instance input Unknown"
        );
        assert_eq!(instance_agg.cost_usd, None);

        // Supply(A): ZERO proven events, but the ambiguity is carried so the
        // provider endpoint still emits the model row with Unknown totals.
        let supply_a = aggregate_supply(&conn, profile, model_a).unwrap();
        assert_eq!(supply_a.events, 0);
        assert_eq!(supply_a.ambiguous_events, 1);
        assert!(supply_a.ambiguous, "model A is ambiguous-only, not absent");
        assert_eq!(supply_a.input_tokens, None);
        assert_eq!(supply_a.cost_usd, None);
        assert_eq!(
            supply_a.budget_status(Some(10.0)),
            BudgetStatus::Unknown,
            "an ambiguous bill can never be certified Ok under budget"
        );

        // Supply(B): the proven stock only.
        let supply_b = aggregate_supply(&conn, profile, model_b).unwrap();
        assert_eq!(supply_b.events, 1);
        assert_eq!(supply_b.ambiguous_events, 0);
        assert!(!supply_b.ambiguous);
        assert_eq!(supply_b.input_tokens, Some(250));
        assert_eq!(supply_b.cost_usd, Some(3.0));
        assert_eq!(supply_b.budget_status(Some(10.0)), BudgetStatus::Ok);

        // Endpoint emission predicate: A is not dropped despite events == 0.
        let endpoint_emits = |agg: &UsageAggregate| agg.events != 0 || agg.ambiguous;
        assert!(
            endpoint_emits(&supply_a),
            "ambiguous-only model row is emitted"
        );
        assert!(endpoint_emits(&supply_b));
        assert!(
            !endpoint_emits(&aggregate_supply(&conn, profile, "model-absent").unwrap()),
            "a model with genuinely no usage still produces no row"
        );
    }
}
